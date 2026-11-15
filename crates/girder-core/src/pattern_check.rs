//! Pattern, enum, and length checks for the string keywords of a tool's JSON Schema.
//!
//! [`crate::tool_contracts`] deliberately leaves `pattern`, `minLength`, and `maxLength`
//! unenforced, and says so: a half-implemented constraint is worse than a clear refusal. The
//! consequence is that a tool declaring `"\u{5e}[a-z]{...}"`-style constraints on a field accepts
//! anything, including `../../etc/passwd`, and nobody notices until the value reaches a filesystem
//! call. This module supplies the missing check.
//!
//! # Why there is no regular-expression engine here
//!
//! Pulling in a backtracking engine would be the obvious move and is the wrong one for this job.
//! Tool schemas are authored in this repository, so the patterns that actually occur are a handful
//! of shapes: an anchored identifier (`^[a-z][a-z0-9_]*$`), a bracketed class, a prefix test with
//! `.*`. A full engine accepts every pattern in the ECMA-262 grammar, which means it also accepts
//! `(?<name>...)`, backreferences, and lookarounds -- constructs whose semantics a reader of the
//! schema cannot verify from the source and whose cost a long value can turn quadratic. Worse, an
//! engine that supports everything gives the schema author no signal about which of those
//! constructs are *checked* and which merely happen to parse.
//!
//! The subset below is small enough to read in one sitting and to prove correct: a literal, a
//! character class, `.`, each optionally repeated with `*` or `+`, with `^` and `$` as anchors.
//! Every other construct is rejected as [`Outcome::BadPattern`] with a pointer to the supported
//! spelling, because the failure mode this module exists to prevent is a schema that looks
//! restrictive and enforces nothing.
//!
//! # Why characters and not bytes
//!
//! Tool arguments carry Chinese text, and every CJK character is three bytes of UTF-8. A byte-wise
//! `[a-z]` test still works by accident, but a byte-wise length under `minLength: 3` would let a
//! single character through as if it were three, and byte arithmetic on a `&str` also invites
//! slicing at a non-boundary, which panics. Everything here iterates `char`, so "one character" in
//! a pattern and "one character" in a length limit mean the same thing.
//!
//! # Why the anchors matter
//!
//! JSON Schema takes an unanchored pattern, and so does ECMA-262: `[a-z]+` is satisfied by `a` in
//! `9a9`. That is the specification's behaviour and this module preserves it, because a caller that
//! anchors `^[a-z]+$` and a caller that writes `[a-z]+` are asking different questions. The
//! difference is pinned by tests, since it is the single easiest thing to get wrong: a checker that
//! silently anchors every pattern accepts `abc` and rejects a legitimate `abc-def` in the same way
//! an engine would, but a checker that silently anchors *nothing* is indistinguishable from no
//! check at all.

use std::fmt;

/// A pattern-check result.
///
/// The three cases are distinguished because the party who can fix each one is different: [`Ok`]
/// needs nothing, [`NoMatch`] means the *caller* sent a value the schema rejects, and
/// [`BadPattern`] means the *schema* is wrong and no caller could ever satisfy it. Collapsing the
/// last two would blame a model for a typo in a checked-in schema.
///
/// [`Ok`]: Outcome::Ok
/// [`NoMatch`]: Outcome::NoMatch
/// [`BadPattern`]: Outcome::BadPattern
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Outcome {
    /// The value matches the pattern.
    Ok,

    /// The pattern is valid and the value does not match it.
    ///
    /// Both sides are carried so a diagnostic can quote the pair without the caller re-threading
    /// the inputs it already passed in.
    NoMatch {
        /// The pattern that was checked.
        pattern: String,
        /// The value that failed it.
        value: String,
    },

    /// The pattern is not in the supported subset, or is malformed.
    ///
    /// `detail` names the offending construct and, where one exists, the supported spelling.
    BadPattern {
        /// The pattern that could not be parsed.
        pattern: String,
        /// Why it could not be parsed, in terms of the construct that was rejected.
        detail: String,
    },
}

impl fmt::Display for Outcome {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Ok => formatter.write_str("matches"),
            Self::NoMatch { pattern, value } => {
                write!(formatter, "`{value}` does not match `{pattern}`")
            }
            Self::BadPattern { pattern, detail } => {
                write!(
                    formatter,
                    "`{pattern}` is not a supported pattern: {detail}"
                )
            }
        }
    }
}

/// How many times an atom may repeat.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Repeat {
    /// Exactly once: no quantifier.
    One,
    /// Zero or more: `*`.
    ZeroOrMore,
    /// One or more: `+`.
    OneOrMore,
}

/// A single-character predicate.
#[derive(Clone, Debug, PartialEq, Eq)]
enum AtomKind {
    /// A literal character, from the pattern source or an escape.
    Literal(char),
    /// `.`: any single character. Newlines included, as ECMA-262 does without the `s`-less
    /// multiline rules this subset does not model.
    Any,
    /// A bracketed class such as `[a-z]` or `[^0-9]`.
    Class(Class),
}

impl AtomKind {
    /// Whether this atom consumes `value`.
    fn accepts(&self, value: char) -> bool {
        match self {
            Self::Literal(expected) => *expected == value,
            Self::Any => true,
            Self::Class(class) => class.contains(value),
        }
    }
}

/// A bracketed character class: a union of inclusive ranges, optionally negated.
///
/// Ranges rather than a member set because `[a-z]` is the common spelling and expanding it would
/// make the parsed form larger than the source without making it clearer.
#[derive(Clone, Debug, PartialEq, Eq)]
struct Class {
    /// Whether the class matches everything it does *not* list.
    negated: bool,
    /// Inclusive `(low, high)` ranges; a single character is stored as `(c, c)`.
    ranges: Vec<(char, char)>,
}

impl Class {
    /// Whether `value` falls inside the class, before or after negation.
    fn contains(&self, value: char) -> bool {
        let listed = self
            .ranges
            .iter()
            .any(|(low, high)| (*low..=*high).contains(&value));
        listed != self.negated
    }
}

/// One atom plus its quantifier.
#[derive(Clone, Debug, PartialEq, Eq)]
struct Atom {
    /// What a single application consumes.
    kind: AtomKind,
    /// How many applications are allowed.
    repeat: Repeat,
}

/// A parsed pattern.
#[derive(Clone, Debug, PartialEq, Eq)]
struct Pattern {
    /// Whether the match must begin at the first character.
    anchored_start: bool,
    /// Whether the match must reach the last character.
    anchored_end: bool,
    /// The atoms, in source order.
    atoms: Vec<Atom>,
}

/// Checks that `value` matches `pattern`.
///
/// The supported subset is a literal, a character class (`[a-z]`, `[A-Z]`, `[0-9]`, `[^...]`), and
/// `.`, each optionally followed by `*` or `+`, with `^` at the start and `$` at the end as
/// anchors. A backslash escapes the following punctuation character, so `\.` is a literal dot.
/// Everything else -- `?`, `{n,m}`, `|`, groups, `\d`/`\w`/`\s` -- is rejected rather than
/// ignored.
///
/// Anchoring follows JSON Schema: without `^` and `$` the match may start and end anywhere, so
/// `check("b+", "abbc")` is [`Outcome::Ok`]. Add both anchors to require the whole value.
///
/// # Errors
///
/// Reports [`Outcome::BadPattern`] for a construct outside the subset and for a malformed one,
/// including a trailing `\`, an unclosed `[`, an empty `[]`, and a reversed range such as `[z-a]`.
#[must_use]
pub fn check(pattern: &str, value: &str) -> Outcome {
    let parsed = match parse(pattern) {
        Ok(parsed) => parsed,
        Err(detail) => {
            return Outcome::BadPattern {
                pattern: pattern.to_owned(),
                detail,
            };
        }
    };
    if parsed.matches(value) {
        Outcome::Ok
    } else {
        Outcome::NoMatch {
            pattern: pattern.to_owned(),
            value: value.to_owned(),
        }
    }
}

/// Checks that `value` is one of `allowed`.
///
/// A plain membership test: JSON Schema compares `enum` members by JSON equality, and for strings
/// that is exactly this. It exists next to [`check`] so a schema check has one place to look for
/// the string keywords rather than reaching for `Vec::contains` inline.
#[must_use]
pub fn check_enum(allowed: &[&str], value: &str) -> bool {
    allowed.contains(&value)
}

/// Checks `value`'s character length against a minimum and a maximum.
///
/// Returns `None` when the length is acceptable and when neither bound is set, so a caller can use
/// the result as the optional violation directly. The bounds are counted in characters, not bytes,
/// which is what makes `minLength: 3` behave the same for `abc` and `中文测试`.
///
/// The returned message states the observed length as well as the bound, because "too short" alone
/// does not tell the caller by how much, and a model told the gap usually fixes it in one turn.
#[must_use]
pub fn check_length(min: Option<usize>, max: Option<usize>, value: &str) -> Option<String> {
    let length = value.chars().count();
    if let Some(minimum) = min
        && length < minimum
    {
        return Some(format!(
            "the value is {length} characters, which is shorter than the minimum length of \
             {minimum}"
        ));
    }
    if let Some(maximum) = max
        && length > maximum
    {
        return Some(format!(
            "the value is {length} characters, which is longer than the maximum length of {maximum}"
        ));
    }
    None
}

impl Pattern {
    /// Whether `value` contains a match.
    ///
    /// The match is simulated as a set of reachable character positions rather than as a recursive
    /// backtracking search. Two reasons. Backtracking over several `.*` atoms is exponential in the
    /// number of atoms for a value that almost matches, and a long argument would turn one schema
    /// check into a stall. And the recursion depth of a backtracker grows with the length of the
    /// value, so a large argument would overflow the stack -- a panic in the one code path whose
    /// job is to reject bad input. Tracking positions keeps the work at `atoms * value` steps of
    /// plain set arithmetic and the depth at zero.
    ///
    /// Positions only ever move forward, which is what makes the repetition closures below a single
    /// left-to-right sweep instead of a fixpoint loop.
    fn matches(&self, value: &str) -> bool {
        let chars: Vec<char> = value.chars().collect();
        // A position is an index into `chars`, so `chars.len()` is the position just past the end.
        let mut reachable = vec![true; chars.len() + 1];
        if self.anchored_start {
            // Only position zero is a legal start; every other entry is a shorter prefix.
            for (index, entry) in reachable.iter_mut().enumerate() {
                *entry = index == 0;
            }
        }
        for atom in &self.atoms {
            reachable = advance(&reachable, &chars, atom);
        }
        if self.anchored_end {
            // `$` is only accepted at the very end of the value.
            reachable[chars.len()]
        } else {
            reachable.iter().any(|entry| *entry)
        }
    }
}

/// Applies one atom, with its quantifier, to the set of reachable positions.
fn advance(reachable: &[bool], chars: &[char], atom: &Atom) -> Vec<bool> {
    match atom.repeat {
        Repeat::One => single(reachable, chars, &atom.kind),
        // The closure of the *current* set: staying put is the "zero" application.
        Repeat::ZeroOrMore => close(reachable.to_vec(), chars, &atom.kind),
        // One application first, then the same closure, which together mean "one or more". Doing
        // it in this order is what keeps `.+` from matching an empty value.
        Repeat::OneOrMore => close(single(reachable, chars, &atom.kind), chars, &atom.kind),
    }
}

/// The positions reachable by applying `atom` exactly once.
fn single(reachable: &[bool], chars: &[char], atom: &AtomKind) -> Vec<bool> {
    let mut next = vec![false; reachable.len()];
    for (position, entry) in reachable.iter().enumerate() {
        if *entry && position < chars.len() && atom.accepts(chars[position]) {
            next[position + 1] = true;
        }
    }
    next
}

/// The positions reachable by applying `atom` zero or more times from `start`.
///
/// A single left-to-right sweep suffices: every application moves the position forward by one, so
/// when the sweep reaches a position, no later step can revisit it and its value is final.
fn close(mut start: Vec<bool>, chars: &[char], atom: &AtomKind) -> Vec<bool> {
    for (position, &character) in chars.iter().enumerate() {
        if start[position] && atom.accepts(character) {
            start[position + 1] = true;
        }
    }
    start
}

/// Parses `source` into a [`Pattern`], or returns the reason it is outside the subset.
fn parse(source: &str) -> Result<Pattern, String> {
    let chars: Vec<char> = source.chars().collect();
    let mut index = 0;
    let mut anchored_start = false;
    if chars.first() == Some(&'^') {
        anchored_start = true;
        index = 1;
    }
    let mut anchored_end = false;
    let mut atoms = Vec::new();
    while index < chars.len() {
        let character = chars[index];
        if character == '$' {
            if index + 1 != chars.len() {
                return Err(
                    "`$` anchors the end of the value, so it may only appear at the end of the \
                     pattern; escape it as `\\$` to match a literal dollar sign"
                        .to_owned(),
                );
            }
            anchored_end = true;
            index += 1;
            continue;
        }
        let (kind, next) = parse_atom(&chars, index)?;
        index = next;
        let (repeat, next) = parse_quantifier(&chars, index)?;
        index = next;
        atoms.push(Atom { kind, repeat });
    }
    Ok(Pattern {
        anchored_start,
        anchored_end,
        atoms,
    })
}

/// Parses the atom starting at `index`, returning it and the index just past it.
fn parse_atom(chars: &[char], index: usize) -> Result<(AtomKind, usize), String> {
    let Some(&character) = chars.get(index) else {
        return Err("the pattern ends where an atom was expected".to_owned());
    };
    match character {
        '.' => Ok((AtomKind::Any, index + 1)),
        '[' => {
            let (class, next) = parse_class(chars, index)?;
            Ok((AtomKind::Class(class), next))
        }
        '\\' => {
            let (literal, next) = parse_escape(chars, index)?;
            Ok((AtomKind::Literal(literal), next))
        }
        // A quantifier or alternation with nothing in front of it is a schema bug, not a literal.
        '*' | '+' => Err(format!(
            "the quantifier `{character}` at offset {index} has nothing to repeat"
        )),
        '?' => Err(
            "`?` is not supported: this checker implements `*` and `+` only, because an optional \
             atom in a schema is clearer as a `|` the schema author writes out and this subset \
             refuses"
                .to_owned(),
        ),
        '{' | '}' => Err(
            "counted repetition such as `{2,4}` is not supported; spell the repeated atom out, \
             e.g. `[a-z][a-z]` for exactly two"
                .to_owned(),
        ),
        '|' => Err(
            "alternation with `|` is not supported; give each alternative its own schema entry"
                .to_owned(),
        ),
        '(' | ')' => Err(
            "groups are not supported; this subset has no capture and no nesting, so a group would \
             only be a place for the checker and the schema author to disagree"
                .to_owned(),
        ),
        '^' => Err(
            "`^` anchors the start of the value, so it may only appear at the start of the \
             pattern; escape it as `\\^` to match a literal caret"
                .to_owned(),
        ),
        _ => Ok((AtomKind::Literal(character), index + 1)),
    }
}

/// Parses the quantifier at `index`, if one is present.
fn parse_quantifier(chars: &[char], index: usize) -> Result<(Repeat, usize), String> {
    let repeat = match chars.get(index) {
        Some('*') => Repeat::ZeroOrMore,
        Some('+') => Repeat::OneOrMore,
        _ => return Ok((Repeat::One, index)),
    };
    let next = index + 1;
    // `a**` and `a*+` parse in ECMA-262 and are almost always a mistake; the subset refuses them
    // rather than picking the possessive-or-nested reading the author probably did not mean.
    if let Some(&following) = chars.get(next)
        && (following == '*' || following == '+')
    {
        return Err(format!(
            "the atom at offset {index} carries two quantifiers (`{following}` follows); one is \
             enough"
        ));
    }
    Ok((repeat, next))
}

/// Parses the escape starting at `index`, returning the literal it denotes and the next index.
fn parse_escape(chars: &[char], index: usize) -> Result<(char, usize), String> {
    let Some(&escaped) = chars.get(index + 1) else {
        return Err(
            "the pattern ends with a trailing `\\`; write `\\\\` to match a literal backslash"
                .to_owned(),
        );
    };
    if escaped.is_ascii_alphanumeric() {
        return Err(format!(
            "`\\{escaped}` is a shorthand class this checker does not implement; write the range \
             out, e.g. `[0-9]` for `\\d` or `[0-9A-Za-z_]` for `\\w`"
        ));
    }
    Ok((escaped, index + 2))
}

/// Parses the character class starting at `index` (which holds `[`).
fn parse_class(chars: &[char], index: usize) -> Result<(Class, usize), String> {
    let mut cursor = index + 1;
    let mut negated = false;
    if chars.get(cursor) == Some(&'^') {
        negated = true;
        cursor += 1;
    }
    let mut ranges = Vec::new();
    let mut closed = false;
    while let Some(&character) = chars.get(cursor) {
        if character == ']' {
            if ranges.is_empty() {
                return Err(
                    "the character class is empty (`[]` matches nothing, so it would reject every \
                     value); list a member, or escape a literal `]` as `\\]`"
                        .to_owned(),
                );
            }
            closed = true;
            cursor += 1;
            break;
        }
        let (low, next) = if character == '\\' {
            parse_escape(chars, cursor)?
        } else {
            (character, cursor + 1)
        };
        cursor = next;
        // A `-` is a range only when it sits between two members; at the end of the class it is a
        // literal hyphen, which is how `[a-z-]` is read by every engine.
        if chars.get(cursor) == Some(&'-')
            && let Some(&high) = chars.get(cursor + 1)
            && high != ']'
        {
            let (high, next) = if high == '\\' {
                parse_escape(chars, cursor + 1)?
            } else {
                (high, cursor + 2)
            };
            if high < low {
                return Err(format!(
                    "the character range `{low}-{high}` is reversed; write `{high}-{low}`"
                ));
            }
            ranges.push((low, high));
            cursor = next;
        } else {
            ranges.push((low, low));
        }
    }
    if !closed {
        return Err(
            "the character class is never closed; add the missing `]`, or escape a literal `[` as \
             `\\[`"
                .to_owned(),
        );
    }
    Ok((Class { negated, ranges }, cursor))
}
