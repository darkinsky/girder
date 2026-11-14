//! Filling `{{variable}}` holes in a deployment's prompt template.
//!
//! One prompt template, many workspaces: an operator writes the prose once, and each workspace
//! supplies the values specific to it. This module is the substitution step only. It decides what
//! a reference means and what happens when one cannot be resolved; it does not know where a
//! template came from, nor where the values did, and it never reads the clock or the filesystem —
//! rendering is a pure function of its two arguments.
//!
//! # Why an undefined variable is a hard error
//!
//! The tempting alternative is to substitute the empty string: the prompt still renders, nothing
//! crashes, and a deployment that was configured wrong appears to work. That is the failure this
//! module exists to prevent. A prompt is prose, so a variable that silently vanishes does not
//! produce a visibly broken document — it produces a slightly shorter one. `You are the assistant
//! for {{org}}.` renders as `You are the assistant for .` and the model reads a sentence that is
//! merely odd, not obviously wrong. Nobody notices until behaviour is already strange, and the
//! cause is a field in a config file three layers away.
//!
//! So [`render`] returns [`TemplateError::Undefined`] instead. The failure lands where the
//! template is rendered — startup, or the first assembly — which is cheap and loud, rather than
//! silent and late. This follows upstream's `renderPrompt`
//! (`packages/core/system-prompt/src/index.ts`), which throws on an unknown `{{variable}}` rather
//! than leaving a blank.
//!
//! # The language, in full
//!
//! - `{{name}}` is a reference. `name` is non-empty and made of ASCII letters, digits, and `_`.
//! - `{{{{` is a literal `{{`.
//! - `}}` outside a reference is literal. It needs no escape because it never opens anything.
//! - A lone `{` and a lone `}` are always literal.
//! - Everything else is copied through byte for byte.
//!
//! The grammar is strict: an unescaped `{{` must be a complete, legal reference. Any other use is
//! an error rather than prose — [`TemplateError::Unclosed`] if no `}}` follows, or
//! [`TemplateError::InvalidName`] if one does but the text between the brackets is not a name.
//!
//! # Why `{{{{` for a literal `{{`
//!
//! A template can be *about* templates, or can quote code that happens to contain braces. The
//! escape has to be usable without making every brace in every template a question.
//!
//! Doubling was chosen over a backslash escape because a backslash is itself meaningful in the
//! prose these prompts carry — Windows paths, Markdown, code samples. With `\{`, every backslash
//! would need auditing, and an unbalanced one would be a parse error in text that has nothing to
//! do with templating. Doubling has no such collision: braces only matter in pairs, so `{{{{`
//! costs nothing anywhere braces were not already special.
//!
//! Doubling also composes. Supplying a literal `{{` immediately followed by a real reference is
//! just `{{{{` next to `{{name}}`, written `{{{{{{name}}`; the scanner consumes each escape and
//! each reference independently.
//!
//! Note this is stricter than upstream, which treats a `{{` with no later `}}` as ordinary prose.
//! The trade is deliberate: prose-with-a-stray-brace and a forgotten closing bracket look
//! identical, and the second is a real bug that would otherwise render as truncated text. Requiring
//! the escape makes the two distinguishable, and it is what gives [`TemplateError::Unclosed`] a
//! case to report at all.
//!
//! # Why a name has exactly one rule
//!
//! Names are `[A-Za-z0-9_]+` and nothing else. A looser rule — "anything without braces" — would
//! let names collide with punctuation the surrounding prose cares about (`-`, `.`, and `:` all
//! appear in the text this crate assembles) and would make the accepted set depend on whichever
//! language wrote the template. One narrow rule, checked in one place, is easier to keep true, and
//! it means a name read out of a template can always be stored by [`Variables::set`] and vice
//! versa.
//!
//! # Why values are not re-scanned
//!
//! Substituted text is opaque. If `style` holds `{{workspace}}`, the output contains those eight
//! characters, the reference is not expanded, and it is not an error. Scanning the result again
//! would let one value pull in others, turning flat substitution into a small programming language
//! — and, more importantly, would make the output depend on the *content* of data the template
//! author never saw. A workspace path, branch name, or organisation name that happens to contain
//! `{{...}}` is attacker-influenced in many deployments; expanding it would let that text reach
//! back into the template and read variables it was never given. Rendering once keeps the set of
//! values that can appear in a prompt fixed by the template alone, and makes rendering terminate
//! by construction with no cycle detection to get wrong.
//!
//! # Example
//!
//! ```
//! use girder_core::{Variables, render_template};
//!
//! let mut variables = Variables::new();
//! variables.set("workspace", "/srv/docs")?;
//! variables.set("today", "2026-11-14")?;
//!
//! assert_eq!(
//!     render_template("You work in {{workspace}}; today is {{today}}.", &variables)?,
//!     "You work in /srv/docs; today is 2026-11-14.",
//! );
//! # Ok::<(), girder_core::TemplateError>(())
//! ```

use std::collections::BTreeMap;
use std::fmt;

/// The opening bracket pair of a reference.
const OPEN: &str = "{{";
/// The closing bracket pair of a reference.
const CLOSE: &str = "}}";
/// The text that stands for one literal [`OPEN`].
const ESCAPED_OPEN: &str = "{{{{";
/// How many bytes an escape consumes from the template.
const ESCAPE_LEN: usize = ESCAPED_OPEN.len();

/// How much of an offending reference is echoed back in an error.
///
/// A mistake is identified by its first few characters; echoing the whole template would copy a
/// long prompt into a log line for the sake of one typo.
const MAX_REPORTED: usize = 32;

/// A failure while rendering a template.
///
/// The variants are distinguished because the caller's fix differs: [`TemplateError::Undefined`]
/// means the deployment did not supply a value, while [`TemplateError::Unclosed`] and
/// [`TemplateError::InvalidName`] mean the template text is wrong.
#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum TemplateError {
    /// The template referenced a name that `variables` does not supply.
    ///
    /// This is the error the module is built around: see the module documentation for why it is
    /// not an empty string.
    #[error(
        "undefined prompt variable {name:?} (a deployment must supply it with `Variables::set`; \
         write `{{{{{{{{` for a literal `{{{{`)"
    )]
    Undefined {
        /// The name as written between the braces.
        name: String,
    },

    /// A reference was opened and the template ended, or reached the next `{{`, without a `}}`.
    ///
    /// `at` is a byte offset into the template, so a caller can point at the offending line
    /// without re-scanning.
    #[error(
        "unclosed prompt variable reference starting at byte {at} (a reference is `{{{{name}}}}`; \
         write `{{{{{{{{` for a literal `{{{{`)"
    )]
    Unclosed {
        /// Byte offset of the `{{` that never closed.
        at: usize,
    },

    /// The text between `{{` and `}}` is not a legal variable name.
    ///
    /// Reachable from both ends of the API: this variant when a template writes a bad name, and
    /// the same variant from [`Variables::set`] when the store is handed one. One mistake, one
    /// variant, so a caller that only wants to abort does not match on two shapes of the same
    /// error.
    #[error(
        "invalid prompt variable name {name:?} (names are ASCII letters, digits, and `_`, and must not be empty)"
    )]
    InvalidName {
        /// The offending text, quoted so whitespace and an empty name stay visible.
        name: String,
    },
}

/// The values a template can refer to.
///
/// A flat string-to-string map, kept ordered so that [`Variables::names`] enumerates names in a
/// stable order: a diagnostic that reorders itself between runs is hard to diff and hard to test.
///
/// Setting a name twice replaces the earlier value. That is what makes layered defaults — "the
/// deployment's value, overridden by this workspace's" — expressible without the caller tracking
/// which layer won.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Variables {
    /// Ordered so enumeration is deterministic; see the type documentation.
    values: BTreeMap<String, String>,
}

impl Variables {
    /// Creates an empty set of variables.
    ///
    /// Every reference rendered against it is [`TemplateError::Undefined`], which is the correct
    /// outcome for a template that expects values and was given none.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Sets `name` to `value`, replacing any previous value.
    ///
    /// The value may be anything, including the empty string and including text that looks like a
    /// reference — see the module documentation on why values are never re-scanned.
    ///
    /// # Errors
    ///
    /// [`TemplateError::InvalidName`] when `name` is empty or contains anything outside
    /// `[A-Za-z0-9_]`. The name is checked here rather than at render time so that it is validated
    /// by the code that chose it, while that code still knows what it meant; and so that a stored
    /// name can always be referred to by a template.
    pub fn set(&mut self, name: &str, value: &str) -> Result<(), TemplateError> {
        if !is_valid_name(name) {
            return Err(TemplateError::InvalidName {
                name: name.to_owned(),
            });
        }
        self.values.insert(name.to_owned(), value.to_owned());
        Ok(())
    }

    /// Returns the names that have been set, in sorted order.
    ///
    /// Intended for diagnostics — turning a [`TemplateError::Undefined`] into a message that says
    /// what *was* available — and for tests. The order is alphabetical, not insertion order, so a
    /// caller must not read it as a history.
    #[must_use]
    pub fn names(&self) -> Vec<&str> {
        self.values.keys().map(String::as_str).collect()
    }

    /// Returns the value set for `name`, if any.
    ///
    /// Exists so a caller can check a conditionally-required variable before rendering, instead of
    /// provoking [`TemplateError::Undefined`] to find out.
    #[must_use]
    pub fn get(&self, name: &str) -> Option<&str> {
        self.values.get(name).map(String::as_str)
    }

    /// Returns whether no variable has been set.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.values.is_empty()
    }

    /// Returns how many variables have been set.
    #[must_use]
    pub fn len(&self) -> usize {
        self.values.len()
    }
}

impl fmt::Display for Variables {
    /// Renders the names, not the values.
    ///
    /// Values are workspace-specific — paths, branch names, sometimes a secret in a misconfigured
    /// deployment — and a `Variables` that lands in a log line should not drag them along. The
    /// names are what a diagnosis needs anyway.
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("Variables { ")?;
        formatter.write_str(&self.names().join(", "))?;
        formatter.write_str(" }")
    }
}

/// Renders a template by substituting its `{{name}}` references.
///
/// The grammar is documented on the module: `{{name}}` is a reference, `{{{{` is a literal `{{`,
/// and a `}}` outside a reference is literal. Substituted values are copied verbatim and never
/// scanned again, so a value containing `{{...}}` appears literally in the output — that is the
/// injection-safety property, not an omission.
///
/// # Errors
///
/// - [`TemplateError::Undefined`] — a reference names something `variables` does not supply.
/// - [`TemplateError::Unclosed`] — a `{{` is not followed by a `}}` before the end of the template.
/// - [`TemplateError::InvalidName`] — the text between `{{` and `}}` is not a legal name.
///
/// Nothing is partially returned: a template either renders completely or fails.
///
/// # Examples
///
/// ```
/// use girder_core::{Variables, render_template};
///
/// let mut variables = Variables::new();
/// variables.set("org", "Acme")?;
///
/// assert_eq!(render_template("For {{org}}.", &variables)?, "For Acme.");
/// // The escape produces braces that are not re-parsed.
/// assert_eq!(render_template("{{{{org}}", &variables)?, "{{org}}");
/// // `}}` outside a reference, and a lone `{`, are literal.
/// assert_eq!(render_template("a }} b { c", &variables)?, "a }} b { c");
/// # Ok::<(), girder_core::TemplateError>(())
/// ```
pub fn render(template: &str, variables: &Variables) -> Result<String, TemplateError> {
    // The common case emits the template almost unchanged, so this is the right starting size.
    let mut rendered = String::with_capacity(template.len());
    let mut cursor = 0;

    while let Some(offset) = template[cursor..].find(OPEN) {
        let open = cursor + offset;

        // Everything before the run is literal, including any lone `}`.
        rendered.push_str(&template[cursor..open]);

        // An escape is consumed as a whole — all four braces — so the second pair does not
        // immediately re-open a reference. That is what makes `{{{{name}}` render the literal
        // `{{name}}` rather than the value of `name`.
        if template[open..].starts_with(ESCAPED_OPEN) {
            rendered.push_str(OPEN);
            cursor = open + ESCAPE_LEN;
            continue;
        }

        // From here the grammar is strict: this `{{` must be a complete, legal reference.
        let body_start = open + OPEN.len();
        let Some(closing) = template[body_start..].find(CLOSE) else {
            return Err(TemplateError::Unclosed { at: open });
        };
        let name = &template[body_start..body_start + closing];

        if !is_valid_name(name) {
            return Err(TemplateError::InvalidName { name: report(name) });
        }

        // Values are pushed as-is and never rescanned; see the module documentation.
        let value = variables
            .get(name)
            .ok_or_else(|| TemplateError::Undefined {
                name: name.to_owned(),
            })?;
        rendered.push_str(value);

        cursor = body_start + closing + CLOSE.len();
    }

    rendered.push_str(&template[cursor..]);
    Ok(rendered)
}

/// Truncates offending text for an error message, keeping it on one line and bounded in length.
///
/// Error text is read by humans scanning a terminal, so a name that is really "the rest of the
/// prompt because a brace was forgotten" must not be echoed in full.
fn report(name: &str) -> String {
    let mut text: String = name.chars().take(MAX_REPORTED).collect();
    if name.chars().count() > MAX_REPORTED {
        text.push('…');
    }
    text
}

/// Returns whether `name` may be used as a variable name.
///
/// The single definition of the rule, shared by [`Variables::set`] and [`render`]. Keeping it in
/// one function is what guarantees that anything a template may reference can also be stored, and
/// that a stored name can always be referenced.
fn is_valid_name(name: &str) -> bool {
    !name.is_empty() && name.chars().all(is_name_character)
}

/// Returns whether `character` may appear in a variable name.
fn is_name_character(character: char) -> bool {
    character.is_ascii_alphanumeric() || character == '_'
}
