//! Assembling the system prompt from the parts a deployment has.
//!
//! Upstream builds the system prompt as a registry of ordered *sections*
//! (`packages/core/system-prompt/src/index.ts`): every capability registers prose at a numeric
//! `order`, a dynamic *context* layer carries per-request facts, and `renderPrompt` sorts the
//! sections, drops the empty ones, and joins the survivors with a blank line. Tools are never part
//! of that text — they ride the request as schemas, which is where this crate's
//! [`LlmRequest::system_prompt`](crate::LlmRequest) finally puts them too.
//!
//! This module is the small, dependency-free core of that idea: a caller describes what it has,
//! and gets back one string. It deliberately does not model the registry — plugins here do not
//! hand each other ordered numbers yet — but it keeps the two rules that make the upstream output
//! readable, because recreating them ad hoc at every call site is how prompts turn into soup.
//!
//! # The format, taken from upstream
//!
//! Upstream has no headings at all: a section is plain prose, and the *only* separator is one blank
//! line (`renderPrompt` joins with `'\n\n'`). Environment facts are written as a `Label: value.`
//! sentence, with paths JSON-quoted so a path containing spaces stays unambiguous — see
//! `packages/session/working-directory/src/index.ts`, which renders
//! `Current working directory: "/some/dir".`. This module follows both conventions: prose blocks
//! separated by a blank line, no `#`/`---` decoration (decoration is tokens spent on nothing), and
//! the `Label: value.` shape for the two scalars.
//!
//! The skills line is the one place this module is cheaper than upstream. Upstream renders a
//! catalog entry as ``- `name`: description`` because the description is what the model needs to
//! choose a skill, and that catalog is re-injected as a user message rather than kept in the
//! system prompt. [`Parts::skills`] carries names only, so bullets would be per-entry overhead
//! with nothing in them; the names are comma-separated on one line instead.
//!
//! # Why this order
//!
//! [`assemble`] emits, in this order, the parts that are present:
//!
//! 1. `base` — what this agent is. It comes first because the model reads it first, and every
//!    later sentence is interpreted against it: "the working directory" is the working directory
//!    *of the agent just described*. A deployment's extra instructions must not be able to
//!    reframe that identity by appearing above it.
//! 2. `workspace`, then `skills`, then `today` — the environment. These are facts, and facts make
//!    sense after the frame that says whose facts they are. Skills sit here rather than at the end
//!    because a skill list is a capability declaration like the other facts: what tools and
//!    instructions exist for this run, not an instruction about how to behave.
//! 3. `extra` — deployment-specific notes. They come last because of recency: the tail of a
//!    prompt is what a model weights most heavily, and these are the one-off qualifications
//!    ("this deployment is air-gapped", "always answer in Chinese") that are meant to override the
//!    general text above them. Putting them anywhere else would let the base text, read later,
//!    dilute them.
//!
//! # Why empty parts vanish instead of rendering an empty label
//!
//! A prompt that says `Available skills:` and then nothing is not neutral — it asserts that some
//! exist and the reader is missing them, and it spends tokens to say so. A deployment with no
//! skills, no known workspace, or no date therefore contributes no block at all. This is the same
//! rule as upstream's `filter(text => text.length > 0)`, and it is applied after trimming so that
//! a whitespace-only value is also treated as absent: it would otherwise leave a stray blank line,
//! which is a separator with nothing on either side.
//!
//! When *every* part is empty the result is the empty string, not a placeholder. Upstream's
//! `renderPrompt` returns `''` in that case, and the caller decides what an empty prompt means —
//! here, [`LlmRequest::system_prompt`](crate::LlmRequest) is an `Option`, so the correct behaviour
//! is to send no system field at all rather than a sentence claiming there is nothing to say.
//!
//! # What is deliberately not here
//!
//! No tool descriptions: tool schemas travel in the request, and duplicating their prose in the
//! prompt would let the two drift. No line-numbered code of any kind: this is text assembly only.
//! No interpolation of `{{variables}}` (upstream's `renderPrompt` does that): nothing in this
//! crate produces templates yet, and a template language with no templates is a parser to test for
//! no output.

use std::collections::HashSet;
use std::path::PathBuf;

/// The separator between assembled blocks.
///
/// One blank line, matching upstream's `renderPrompt`. It is enough to end a paragraph and costs
/// two tokens; a rule or a heading would cost more and add nothing the blank line does not.
const BLOCK_SEPARATOR: &str = "\n\n";

/// The system prompt's parts, before assembly.
///
/// Fields are in assembly order, so the struct itself documents what [`assemble`] will emit and in
/// what sequence. Every field is optional in effect: an empty `base`, an absent `workspace`, and an
/// empty list all contribute nothing.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Parts {
    /// The base description: what this agent is.
    ///
    /// Rendered first and verbatim apart from trimming. A deployment that leaves it empty gets a
    /// prompt with no framing at all, which is legal but rarely what it wants.
    pub base: String,
    /// The workspace path, shown as `Current working directory: "<path>".`
    ///
    /// `None` means the deployment has no workspace to name — a one-shot API call, for instance —
    /// and the line is omitted rather than rendered with an empty path.
    pub workspace: Option<PathBuf>,
    /// Names of the skills this deployment can invoke.
    ///
    /// Rendered as one `Available skills: ...` line. Blank names are dropped (nothing can invoke
    /// them) and duplicates are collapsed (a repeated name costs tokens and reads as two skills).
    pub skills: Vec<String>,
    /// The current date, in ISO 8601 (`YYYY-MM-DD`).
    ///
    /// A `String` rather than a date type because the caller owns the clock: this crate has a
    /// calendar in [`crate`]'s time vocabulary, but a prompt builder has no business reading the
    /// wall clock, and formatting at the call site keeps replay deterministic.
    pub today: Option<String>,
    /// Deployment-specific notes, appended last, each as its own block.
    ///
    /// Kept as a list rather than one string so that two independent sources — say, an operator's
    /// note and an environment-derived warning — do not have to agree on how to concatenate.
    pub extra: Vec<String>,
}

impl Parts {
    /// Starts from a base description with everything else empty.
    ///
    /// The remaining fields are public, so this exists only to keep the common call site — "an
    /// agent with a description and nothing else yet" — from spelling out five `Default` values.
    #[must_use]
    pub fn new(base: impl Into<String>) -> Self {
        Self {
            base: base.into(),
            ..Self::default()
        }
    }
}

/// Assembles the parts into one system prompt.
///
/// Blocks are emitted in [`Parts`] field order, joined by exactly one blank line, and only for
/// parts that are non-empty after trimming. The result is `""` when nothing was contributed; see
/// the module documentation for why that is the right answer rather than a placeholder.
///
/// # Examples
///
/// ```
/// use std::path::PathBuf;
///
/// use girder_core::system_prompt::{Parts, assemble};
///
/// let parts = Parts {
///     base: "You are a documentation agent.".to_owned(),
///     workspace: Some(PathBuf::from("/srv/docs")),
///     ..Parts::default()
/// };
///
/// assert_eq!(
///     assemble(&parts),
///     "You are a documentation agent.\n\nCurrent working directory: \"/srv/docs\".",
/// );
/// ```
#[must_use]
pub fn assemble(parts: &Parts) -> String {
    // Sized for the common case so a deployment with notes does not reallocate per block.
    let mut blocks: Vec<String> = Vec::with_capacity(3 + parts.extra.len());

    let base = parts.base.trim();
    if !base.is_empty() {
        blocks.push(base.to_owned());
    }

    if let Some(workspace) = &parts.workspace {
        // `display` rather than an owned conversion: a path that is not valid UTF-8 still has a
        // useful rendering, and refusing to describe the workspace would be worse than an
        // occasional replacement character.
        blocks.push(format!(
            "Current working directory: {}.",
            quote(&workspace.display().to_string())
        ));
    }

    if let Some(skills) = render_skills(&parts.skills) {
        blocks.push(skills);
    }

    if let Some(today) = non_empty(parts.today.as_deref()) {
        blocks.push(format!("Current date: {today}."));
    }

    // Each note is its own block: they come from independent sources and are not a list to browse.
    blocks.extend(
        parts
            .extra
            .iter()
            .filter_map(|note| non_empty(Some(note.as_str())).map(str::to_owned)),
    );

    blocks.join(BLOCK_SEPARATOR)
}

/// Returns `value` when it holds something other than whitespace.
///
/// The whitespace check is what keeps a blank value from becoming a block that renders as an empty
/// paragraph: trimming already happens at the call sites, so this is the single rule they share.
fn non_empty(value: Option<&str>) -> Option<&str> {
    value.map(str::trim).filter(|text| !text.is_empty())
}

/// Renders the skills line, or `None` when there is nothing to say.
///
/// Names are trimmed because a name is what a consumer passes to the `skill` tool; whitespace in it
/// would not match, and the prompt is not the place to fix a provider's front matter.
fn render_skills(skills: &[String]) -> Option<String> {
    let mut seen = HashSet::with_capacity(skills.len());
    let names = skills
        .iter()
        .filter_map(|skill| non_empty(Some(skill.as_str())))
        .filter(|name| seen.insert(*name))
        .map(|name| format!("`{name}`"))
        .collect::<Vec<_>>();

    if names.is_empty() {
        return None;
    }
    Some(format!("Available skills: {}.", names.join(", ")))
}

/// Quotes a string the way upstream's `JSON.stringify` does for a displayed path.
///
/// Quoting is not cosmetic: a path may contain spaces, quotes, or a trailing newline, and an
/// unquoted line would let a path end early and turn the rest of it into prose the model reads as
/// instructions. Only the escapes that can appear in a path are handled — this is a display
/// encoder, not a serializer.
fn quote(text: &str) -> String {
    let mut quoted = String::with_capacity(text.len() + 2);
    quoted.push('"');
    for character in text.chars() {
        match character {
            '"' => quoted.push_str("\\\""),
            '\\' => quoted.push_str("\\\\"),
            '\n' => quoted.push_str("\\n"),
            '\r' => quoted.push_str("\\r"),
            '\t' => quoted.push_str("\\t"),
            other => quoted.push(other),
        }
    }
    quoted.push('"');
    quoted
}
