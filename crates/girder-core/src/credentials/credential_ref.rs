//! Credential *references* and a recognizer that shrinks leaked secrets back to one.
//!
//! [`crate::credentials`] already separates a stored secret from anything a caller may see: its
//! info record has no field a value could ride in. This module covers the other leak path, the one
//! that seam cannot close — a secret that arrives inside free text. A tool prints an `Authorization`
//! header, a subprocess echoes an environment variable, an error message quotes the command line
//! that failed. None of that goes through the credential store, so nothing there can withhold it.
//!
//! # Detection is anchored, not statistical
//!
//! Every rule here is anchored on something the text itself states: a literal prefix a provider
//! issues ([`KNOWN_PREFIXES`]), or an explicit `Bearer` scheme followed by a token. There is no
//! entropy heuristic and no "long opaque string" rule, because those fire on things that are not
//! secrets — digests, request ids, base64 payloads, long paths — and a redactor that mangles
//! diagnostics is a redactor callers learn to switch off. What this misses is a secret with no
//! prefix pasted without a scheme; that is recorded as a deliberate limit rather than papered over
//! with a rule that is wrong more often than right.
//!
//! # Two length floors
//!
//! A prefix alone is not enough: `sk-` is also how people write "sk-<something short>" in a doc, and
//! a one-character body is a placeholder, not a key. So each rule also requires a minimum body
//! length — [`MIN_PREFIXED_BODY_LEN`] after a known prefix, [`MIN_BEARER_TOKEN_LEN`] after `Bearer`.
//! The floors are what keep ordinary prose untouched, and they are checked before anything is
//! rewritten.
//!
//! # A redaction keeps a handle
//!
//! The masked form keeps the leading characters ([`REVEAL_CHARS`] of the body, plus the prefix when
//! there is one): `sk-abcd...`. The whole point of naming a leaked key is to tell the operator
//! *which* one to rotate, and a blanket `***` cannot do that. What is left is a prefix plus four
//! characters — enough to correlate with a listing, far short of a usable secret.
//!
//! # What a pass looks like
//!
//! ```text
//! redact("key: sk-abcdefghijklmnop")       -> ("key: sk-abcd...", 1)
//! redact("the word internationalization")  -> ("the word internationalization", 0)
//! ```
//!
//! The second line is the important one. It is twenty ordinary characters with no anchor, so it is
//! left alone; a redactor that rewrote it would be one a caller stops running.

use std::fmt;

/// The literal prefixes this crate recognizes as the start of a credential.
///
/// These are the forms providers actually issue: `sk-` for OpenAI-style keys, `ghp_` for a personal
/// access token, `glpat-` for a project access token, `xoxb-` for a bot token. The list is public so
/// a deployment can tell from one place what this build recognizes, and so the rule set is data
/// rather than a chain of `starts_with` calls a reader has to reconstruct.
pub const KNOWN_PREFIXES: &[&str] = &["sk-", "ghp_", "glpat-", "xoxb-"];

/// Shortest body accepted after a known prefix.
///
/// Eight characters is well below any issued key and well above a placeholder such as `sk-test` or
/// `sk-xxx`. The floor exists so a document that *mentions* a prefix is not rewritten.
const MIN_PREFIXED_BODY_LEN: usize = 8;

/// Shortest token accepted after a `Bearer` scheme.
///
/// Longer than the prefixed floor because the scheme is the only evidence: `Bearer` is also an
/// ordinary English word, so a short word following it must be read as prose rather than a token.
const MIN_BEARER_TOKEN_LEN: usize = 12;

/// How many characters of the body a masked form keeps.
///
/// Four correlates a redaction with a key listing without leaving a usable fragment.
const REVEAL_CHARS: usize = 4;

/// What replaces the characters that are dropped.
const REDACTION_MARKER: &str = "...";

/// The scheme word a token may be introduced by.
const BEARER_SCHEME: &str = "Bearer";

/// A reference to a credential — not its value.
///
/// The name has the form `provider/key`, where the provider names the issuing system and the key
/// names the credential within it. A reference is safe to log, serialize, and hand to a caller: it
/// names where a value lives without carrying one. Storage and lookup belong to [`crate::credentials`];
/// this type only states the grammar, so a name that reaches a log line is one that could not have
/// been a value.
///
/// # Example (not executed)
///
/// ```text
/// CredentialRef::new("deepseek/api_key")?  -> a reference whose as_str() is "deepseek/api_key"
/// CredentialRef::new("deepseek")           -> Err(RefError::MissingSeparator)
/// ```
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct CredentialRef(String);

impl CredentialRef {
    /// Builds a reference from its name.
    ///
    /// The accepted grammar is one `/` with a non-empty side on each end, and ASCII letters, digits,
    /// `-`, `_`, `.` elsewhere. Slashes, spaces, and non-ASCII characters are refused rather than
    /// escaped: a name is meant to be comparable and loggable verbatim, and an escape rule would
    /// make `a/b` and its escaped spelling two different references to one credential.
    ///
    /// # Errors
    ///
    /// - [`RefError::Empty`] when `name` is empty.
    /// - [`RefError::MissingSeparator`] when there is no `/`, or when a side of it is empty —
    ///   `provider/`, `/key`, and `a//b` all fail here, because each is a name with a missing half
    ///   rather than a name with a stray character.
    /// - [`RefError::InvalidCharacter`] for a character outside the accepted set, including a second
    ///   `/`.
    pub fn new(name: &str) -> Result<Self, RefError> {
        if name.is_empty() {
            return Err(RefError::Empty);
        }

        // Splitting first is what separates "a missing half" from "a stray character". `a//b` splits
        // into three parts, one of them empty, so a side of the separator is missing. `a/b/c` splits
        // into three parts, none empty, so the extra `/` is the offending character. Reporting both
        // as the same error would tell a caller to fix the wrong thing.
        let parts: Vec<&str> = name.split('/').collect();
        if parts.len() == 1 || parts.iter().any(|part| part.is_empty()) {
            return Err(RefError::MissingSeparator);
        }
        if parts.len() > 2 {
            return Err(RefError::InvalidCharacter { character: '/' });
        }

        for character in name.chars().filter(|character| *character != '/') {
            if !is_name_character(character) {
                return Err(RefError::InvalidCharacter { character });
            }
        }

        Ok(Self(name.to_owned()))
    }

    /// The name, exactly as it was validated.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for CredentialRef {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.0)
    }
}

/// Why a name is not a usable credential reference.
///
/// The variants are the three ways a caller's input can be wrong, kept apart because the repair
/// differs: add the missing half, drop the stray character, or supply a name at all.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RefError {
    /// The name is empty, so it would name every credential at once.
    Empty,
    /// The name has no `/`, or a side of it is empty.
    MissingSeparator,
    /// The name contains a character the grammar does not accept.
    InvalidCharacter {
        /// The offending character, reported so the caller can name it in a message.
        character: char,
    },
}

impl fmt::Display for RefError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Empty => formatter.write_str(
                "a credential reference must not be empty: it names one credential as `provider/key`",
            ),
            Self::MissingSeparator => formatter.write_str(
                "a credential reference must be `provider/key`, with a non-empty name on each side \
                 of exactly one `/`",
            ),
            Self::InvalidCharacter { character } => write!(
                formatter,
                "a credential reference may not contain `{}`: only ASCII letters, digits, `-`, `_` \
                 and `.` are allowed, plus one `/` between provider and key",
                character.escape_debug(),
            ),
        }
    }
}

impl std::error::Error for RefError {}

/// Replaces anything that looks like a credential with a masked form that keeps its prefix.
///
/// Returns the possibly rewritten text and how many credentials were found. The count is returned
/// beside the text rather than folded into it so a caller can decide what to do about a leak — warn,
/// refuse to persist, redact in place — without parsing the output to find out whether it happened.
///
/// Two rules are applied, and only two:
///
/// 1. A token beginning with one of [`KNOWN_PREFIXES`] whose body is at least
///    [`MIN_PREFIXED_BODY_LEN`] characters long.
/// 2. A token introduced by a `Bearer` scheme (case-insensitive) that is at least
///    [`MIN_BEARER_TOKEN_LEN`] characters long.
///
/// A token is a maximal run of ASCII letters, digits, and `-`, `_`, `.`, `+`, `~`. The scheme word
/// itself is kept, so `Bearer eyJh...` still reads as an authorization header. A token already in
/// masked form is left alone, which makes this function idempotent.
///
/// If nothing matches, the input is returned unchanged — same bytes, not a re-encoded equivalent —
/// so a caller can compare against the original to detect that rewriting happened.
///
/// # Example (not executed)
///
/// ```text
/// redact("Authorization: Bearer abcdefghijklmnop")
///   -> ("Authorization: Bearer abcd...", 1)
/// redact("a plain twenty-char word: internationalization")
///   -> (unchanged, 0)
/// ```
#[must_use]
pub fn redact(text: &str) -> (String, usize) {
    let mut redacted = String::with_capacity(text.len());
    let mut copied = 0;
    let mut found = 0;

    for (start, end) in token_spans(text) {
        let token = &text[start..end];
        let Some(masked) = masked_form(text, start, token) else {
            continue;
        };
        redacted.push_str(&text[copied..start]);
        redacted.push_str(&masked);
        copied = end;
        found += 1;
    }

    if found == 0 {
        return (text.to_owned(), 0);
    }
    redacted.push_str(&text[copied..]);
    (redacted, found)
}

/// Characters that may appear inside one candidate token.
///
/// Restricted to ASCII on purpose: a key is ASCII, and treating Unicode word characters as part of
/// a token would let a redaction cut through the middle of a sentence in another script.
///
/// `=` and `/` are deliberately excluded. Both are ordinary separators in the text a leak appears
/// in — `GITHUB_TOKEN=ghp_…` is an assignment, not a token that begins with `GITHUB_TOKEN` — and
/// including them would hide the prefix behind whatever assignment introduces it. The cost is that
/// base64 `=` padding falls outside the redaction, which reveals nothing: the padding is not part of
/// the value.
fn is_token_character(character: char) -> bool {
    character.is_ascii_alphanumeric() || matches!(character, '-' | '_' | '.' | '+' | '~')
}

/// Characters a reference name may carry besides the separator.
fn is_name_character(character: char) -> bool {
    character.is_ascii_alphanumeric() || matches!(character, '-' | '_' | '.')
}

/// The maximal runs of token characters in `text`, as byte ranges.
fn token_spans(text: &str) -> Vec<(usize, usize)> {
    let mut spans = Vec::new();
    let mut run_start: Option<usize> = None;

    for (index, character) in text.char_indices() {
        if is_token_character(character) {
            if run_start.is_none() {
                run_start = Some(index);
            }
        } else if let Some(start) = run_start.take() {
            spans.push((start, index));
        }
    }
    if let Some(start) = run_start {
        spans.push((start, text.len()));
    }

    spans
}

/// The masked replacement for one token, or `None` when the token is not a credential.
///
/// Split out so both rules are read in one place: what is tested is the token plus the text before
/// it, and what comes back is only ever a shorter string.
fn masked_form(text: &str, start: usize, token: &str) -> Option<String> {
    if let Some(prefix) = KNOWN_PREFIXES
        .iter()
        .find(|prefix| token.starts_with(**prefix))
    {
        let body = &token[prefix.len()..];
        if body.len() >= MIN_PREFIXED_BODY_LEN && !is_already_masked(body) {
            return Some(masked(token, prefix.len() + REVEAL_CHARS));
        }
        return None;
    }

    if token.len() >= MIN_BEARER_TOKEN_LEN
        && !is_already_masked(token)
        && bearer_precedes(text, start)
    {
        return Some(masked(token, REVEAL_CHARS));
    }

    None
}

/// Whether the text immediately before `start` is a `Bearer` scheme plus at least one space.
///
/// The word boundary check matters: without it, `NotBearer <token>` would redact, and so would any
/// identifier that happens to end in the scheme.
fn bearer_precedes(text: &str, start: usize) -> bool {
    let before = &text[..start];
    let trimmed = before.trim_end_matches([' ', '\t']);
    if trimmed.len() == before.len() {
        return false;
    }

    let Some(tail) = trimmed.get(trimmed.len().saturating_sub(BEARER_SCHEME.len())..) else {
        return false;
    };
    if !tail.eq_ignore_ascii_case(BEARER_SCHEME) {
        return false;
    }

    let word_start = trimmed.len() - BEARER_SCHEME.len();
    trimmed[..word_start]
        .chars()
        .next_back()
        .is_none_or(|character| !character.is_ascii_alphanumeric())
}

/// Whether a body is already in the masked form this module emits.
///
/// Checked so a second pass cannot eat what a first pass left behind: without it, `sk-abcd...`
/// would eventually redact its own reveal prefix.
fn is_already_masked(body: &str) -> bool {
    body.ends_with(REDACTION_MARKER)
}

/// Keeps `keep` bytes of `token` and appends the marker.
///
/// `keep` is clamped to the token length; the slice is safe because every token character is ASCII,
/// so a byte offset is always a character boundary.
fn masked(token: &str, keep: usize) -> String {
    let keep = keep.min(token.len());
    let mut masked = String::with_capacity(keep + REDACTION_MARKER.len());
    masked.push_str(&token[..keep]);
    masked.push_str(REDACTION_MARKER);
    masked
}
