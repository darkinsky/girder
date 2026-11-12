//! Shape checks for a provider API key, run before the key ever reaches a request.
//!
//! Upstream's `api-key.ts` makes one point that this module keeps: a bad key is a *configuration*
//! defect, and the worst place to discover it is inside a provider's 401. A 401 says "credentials
//! rejected" and says nothing about *why*, so the operator is left guessing between a revoked key,
//! a wrong project, a quota problem, and — very often — a key that was pasted with the surrounding
//! quotes still attached. The check here separates the last class from the rest while the answer is
//! still local and the fix is still obvious.
//!
//! # What this module is not
//!
//! It does not verify the key. Nothing here talks to a provider, and a key that passes every checks
//! below can still be revoked, misscoped, or simply wrong. `KeyCheck::Ok` means *nothing about the
//! shape is obviously broken*, not *the key works*. Upstream draws the same line, for the same
//! reason: a checks that needs the network is a checks that cannot run in the common failing path
//! (offline, wrong proxy, unreachable endpoint), which is exactly where it would be most useful.
//!
//! # Why the checks are ordered the way they are
//!
//! [`check`] reports one problem, not a list, because the first defect almost always explains the
//! rest: a key wrapped in quotes will also look "too short" if the quotes are counted, and a key
//! with a `Bearer ` prefix will also trip the non-ASCII test once a non-ASCII space follows the
//! word. Reporting the most specific, most actionable defect first keeps the message honest.
//!
//! # Why repair is separate from judgement
//!
//! [`normalize`] fixes the defects that have exactly one plausible reading — surrounding
//! whitespace, a stray `Bearer ` prefix, a pair of wrapping quotes — and changes nothing else. It
//! is deliberately not "try things until the key looks legal": silently rewriting a key that is
//! genuinely wrong would turn a loud configuration error into a quiet authentication failure, and
//! the operator would have no way to learn that the harness edited their credential. Callers that
//! want both behaviours ask [`check`] first and [`normalize`] only for the defects it names.

/// The verdict on one supplied API key.
///
/// Carries the explanation rather than a code because the two callers want different things: a
/// hard refusal wants [`KeyCheck::Suspect::problem`] to log, a warning wants
/// [`KeyCheck::Suspect::hint`] to show. A caller that only needs a boolean can use
/// [`KeyCheck::is_ok`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum KeyCheck {
    /// Nothing about the shape is obviously broken.
    ///
    /// This is *not* a statement that the key is valid — see the module documentation.
    Ok,

    /// The key is visibly wrong in a way that can be named.
    Suspect {
        /// What was observed, phrased so it can be logged as-is.
        problem: String,

        /// What to do about it, phrased as an instruction to the operator.
        ///
        /// Always non-empty, and always something the operator can act on: "paste the key
        /// without the quotes", not "invalid character at offset 0".
        hint: String,
    },
}

impl KeyCheck {
    /// Returns `true` for [`KeyCheck::Ok`].
    ///
    /// Provided so a caller that only gates on the verdict does not have to destructure.
    #[must_use]
    pub fn is_ok(&self) -> bool {
        matches!(self, Self::Ok)
    }
}

/// The shortest key this module will accept.
///
/// Every provider in scope issues keys that are at least this long: `DeepSeek` and Anthropic keys are
/// both well over 30 characters, and OpenAI-style keys over 40. 10 is not a guess at a real length,
/// it is a floor far below any of them: a 4-character key is a truncation, a placeholder left in a
/// config file, or a value that got cut at a `#`, and each of those deserves a local message rather
/// than a provider's 401. A real key that is somehow shorter would still pass every other check
/// here and fail at the provider, which is the pre-existing behaviour.
pub const MIN_KEY_LEN: usize = 10;

/// The prefix that turns a key into an `Authorization` header value.
///
/// Matched case-insensitively so that a key pasted from a shell variable that lowercased the
/// scheme is still recognized as a pasted header rather than reported as a non-ASCII defect.
const BEARER_PREFIX: &str = "bearer ";

/// Checks the shape of one API key.
///
/// The checks run in this order, most specific first:
///
/// 1. Empty or all whitespace — nothing was configured.
/// 2. Surrounding whitespace — a paste artifact, repairable by [`normalize`].
/// 3. A `Bearer ` prefix — the whole header value was pasted instead of the key.
/// 4. A wrapping pair of quotes — a value copied out of JSON or a shell literal, repairable by
///    [`normalize`]. A lone quote elsewhere is reported too, but not offered for repair.
/// 5. Any non-ASCII character — no provider issues one, and no header can carry it.
/// 6. Shorter than [`MIN_KEY_LEN`] — almost always a truncated or placeholder value.
///
/// # Examples
///
/// ```
/// use girder_core::api_key::{check, KeyCheck};
///
/// assert_eq!(check("sk-abcdefghijklmnop"), KeyCheck::Ok);
///
/// match check("\"sk-abcdefghijklmnop\"") {
///     KeyCheck::Suspect { problem, hint } => {
///         assert!(problem.contains("quote"));
///         assert!(!hint.is_empty());
///     }
///     KeyCheck::Ok => panic!("a quoted key must not pass"),
/// }
/// ```
#[must_use]
pub fn check(raw: &str) -> KeyCheck {
    if raw.trim().is_empty() {
        return suspect(
            "the API key is empty or contains only whitespace",
            "Set the key to the value the provider issued, for example \
             `DEEPSEEK_API_KEY=sk-...`. An empty value is treated as unset, so the request cannot \
             authenticate.",
        );
    }

    // Checked before the content checks because it is the single most common paste defect and the
    // one whose repair is unambiguous: trimming cannot change which key was meant.
    let trimmed = raw.trim();
    if trimmed.len() != raw.len() {
        return suspect(
            "the API key has leading or trailing whitespace",
            "Trim the value before storing it. Surrounding whitespace is almost always a copy \
             artifact, and some clients reject a header built from it. Girder can normalize this \
             with `api_key::normalize`.",
        );
    }

    if is_bearer_prefixed(raw) {
        return suspect(
            "the API key starts with a `Bearer ` prefix",
            "Store only the key itself, not the whole `Authorization` header value. Paste the \
             text after `Bearer ` — Girder adds the scheme when it builds the header.",
        );
    }

    if has_wrapping_quotes(raw) {
        return suspect(
            "the API key is wrapped in matching quotes",
            "Strip the surrounding quotes. This happens when a key is copied out of a JSON file \
             or a shell literal; the quotes are not part of the key. Girder can normalize this \
             with `api_key::normalize`.",
        );
    }

    if let Some(ch) = raw.chars().find(|ch| is_quote(*ch)) {
        return suspect(
            &format!("the API key contains the quote character {ch:?}"),
            "Remove the quote. Keys are unquoted alphanumerics and punctuation; a quote that \
             survived the copy is either an unbalanced wrapper or a shell/JSON escape artifact. \
             Girder cannot repair this one automatically, because it cannot tell which side of \
             the key the missing partner belonged to.",
        );
    }

    if let Some((offset, ch)) = first_non_ascii(raw) {
        return suspect(
            &format!("the API key contains the non-ASCII character {ch:?} at byte offset {offset}"),
            "Re-enter the key as plain ASCII, or re-copy it from the provider console. A \
             non-ASCII character is almost always a smart quote, a dash, or a non-breaking space \
             introduced by a word processor or a chat client, and cannot be sent in an HTTP \
             header.",
        );
    }

    if raw.len() < MIN_KEY_LEN {
        return suspect(
            &format!(
                "the API key is only {} characters long, below the {MIN_KEY_LEN}-character floor",
                raw.len()
            ),
            "Check that the key was copied in full and that no placeholder such as `changeme` or \
             `<your-key>` was left in the configuration. Provider keys are much longer than this.",
        );
    }

    KeyCheck::Ok
}

/// Normalizes a key into the shape that can be sent as-is.
///
/// Repairs exactly the defects [`check`] names as repairable, in the order they are reported:
/// surrounding whitespace, a leading `Bearer `, then a wrapping pair of quotes. Nothing else is
/// touched, so a key that was already well formed comes back byte-identical.
///
/// The repair is repeated until it reaches a fixed point because the defects compose: a value
/// copied from a JSON file may be `"sk-...\n"` — quotes inside whitespace, or whitespace inside
/// quotes — and a single pass in a fixed order would leave one of them behind. The loop is
/// bounded by the length of the input: every pass that changes the string removes at least one
/// character, so it cannot spin.
///
/// This function never reports failure: a string it cannot repair is returned unchanged, and the
/// caller is expected to run [`check`] on the result to learn whether the key is usable. That
/// separation is deliberate — normalize answers "what did the operator most likely mean",
/// `check` answers "is this usable".
///
/// # Examples
///
/// ```
/// use girder_core::api_key::{check, normalize, KeyCheck};
///
/// assert_eq!(normalize("  sk-abcdefghijklmnop \n"), "sk-abcdefghijklmnop");
/// assert_eq!(normalize("Bearer sk-abcdefghijklmnop"), "sk-abcdefghijklmnop");
/// assert_eq!(normalize("\"sk-abcdefghijklmnop\""), "sk-abcdefghijklmnop");
///
/// // A key that was already well formed is not disturbed.
/// assert_eq!(normalize("sk-abcdefghijklmnop"), "sk-abcdefghijklmnop");
/// assert_eq!(check(&normalize(" \"sk-abcdefghijklmnop\"\r\n")), KeyCheck::Ok);
/// ```
#[must_use]
pub fn normalize(raw: &str) -> String {
    let mut value = raw.to_owned();
    loop {
        let before = value.len();
        value = value.trim().to_owned();
        value = strip_bearer(&value).to_owned();
        value = strip_wrapping_quotes(&value).to_owned();
        if value.len() == before {
            return value;
        }
    }
}

/// Builds a [`KeyCheck::Suspect`] from a problem and a hint.
///
/// A free function rather than a `KeyCheck` constructor so the variants stay the whole public
/// surface; callers match on the enum and never build it.
fn suspect(problem: &str, hint: &str) -> KeyCheck {
    KeyCheck::Suspect {
        problem: problem.to_owned(),
        hint: hint.to_owned(),
    }
}

/// Returns `true` when `value` opens with a case-insensitive `Bearer `.
fn is_bearer_prefixed(value: &str) -> bool {
    value.len() >= BEARER_PREFIX.len()
        && value
            .get(..BEARER_PREFIX.len())
            .is_some_and(|head| head.eq_ignore_ascii_case(BEARER_PREFIX))
}

/// Removes one leading `Bearer `, if present.
///
/// Only the prefix is removed; the key that follows is left exactly as it was, including any
/// whitespace, so the surrounding-whitespace pass on the next iteration sees it.
fn strip_bearer(value: &str) -> &str {
    if is_bearer_prefixed(value) {
        value.get(BEARER_PREFIX.len()..).unwrap_or(value)
    } else {
        value
    }
}

/// Returns `true` when `ch` is one of the quote characters [`has_wrapping_quotes`] recognizes.
fn is_quote(ch: char) -> bool {
    matches!(ch, '"' | '\'' | '`')
}

/// Returns `true` when `value` is wrapped in a matching pair of `"`, `'`, or `` ` ``.
///
/// A single quote character *inside* the key is not a wrapping quote: the pair must match and must
/// enclose something, which is why the length test is `> 2` rather than `>= 2`.
fn has_wrapping_quotes(value: &str) -> bool {
    let bytes = value.as_bytes();
    bytes.len() > 2
        && matches!(bytes.first(), Some(b'"' | b'\'' | b'`'))
        && bytes.first() == bytes.last()
}

/// Removes one wrapping pair of quotes, if present.
fn strip_wrapping_quotes(value: &str) -> &str {
    if has_wrapping_quotes(value) {
        value.get(1..value.len() - 1).unwrap_or(value)
    } else {
        value
    }
}

/// Returns the byte offset and value of the first non-ASCII character in `value`, if any.
///
/// The offset is a byte offset rather than a character index because that is what survives logging
/// and diffing a configuration file: a character index shifts as soon as one non-ASCII character is
/// removed, while a byte offset keeps pointing at the same place in the original string.
fn first_non_ascii(value: &str) -> Option<(usize, char)> {
    value.char_indices().find(|(_offset, ch)| !ch.is_ascii())
}
