//! Skill invocation policy and the model-facing catalog protocol.
//!
//! # Where this comes from
//!
//! Upstream `packages/skill` is 2,845 production lines across five packages. The capability this
//! module supplies is the one the local port has no representation of at all, spread over three of
//! them:
//!
//! | upstream symbol | file |
//! |---|---|
//! | `isSkillName`, `SKILL_NAME` | `skill/src/index.ts` |
//! | `SkillInvocationPolicy`, `isModelInvocable`, `isUserInvocable` | `skill/src/index.ts` |
//! | `escapeText`, `escapeAttr` | `skill/src/index.ts` |
//! | `SkillResourceBase`, `renderSkillContent`, `renderResourceHint` | `skill/src/index.ts` |
//! | `parseFrontmatter`, `parseInvocationPolicy`, `frontmatterBoolean`, `rejectLegacyInvocationKey` | `skill-filesystem/src/index.ts` |
//! | `catalogDescription`, `catalogSourceEntries`, `renderCatalogEntries` | `tool-skill/src/index.ts` |
//! | `renderCatalogMessage`, `renderCatalogUpdate`, `incompleteCatalogNotice` | `tool-skill/src/index.ts` |
//! | `digestCatalogEntries`, `readCatalogEntries`, `SkillCatalogSource` | `tool-skill/src/index.ts` |
//! | `SKILL_GESTURE`, `invokedSkillNames` | `tool-skill/src/index.ts` |
//!
//! # The gap, stated precisely
//!
//! [`crate::SkillRegistry`] merges providers and the local `skill_tool` module lists and loads, but nothing in the
//! local crate knows that a skill can be *unavailable*. Upstream's front matter carries two switches
//! — `disable-model-invocation` and `user-invocable` — and its registry, its tool, and its
//! pre-step catalog all consult them at their own boundary. Locally a skill whose author wrote
//! `disable-model-invocation: true` is advertised in the catalog and loadable by the model, and one
//! whose author wrote `user-invocable: false` can still be invoked by a human. Neither is a missing
//! convenience; both are the author's instruction being ignored.
//!
//! The gap is not only the switches. Upstream renders one canonical `<skill_content>` block for both
//! the tool result and the user-invocation injection, and it publishes the catalog as a framed
//! `<available_skills>` context message whose *replacement* form carries the rule that stale names
//! must be discarded. Local [`crate::render_skill_list`] emits bare bullet lines with no
//! frame and no notion of a catalog that supersedes an earlier one.
//!
//! # Why this is the largest gap, and not the other two
//!
//! The three candidates were checked against the tree rather than taken on report:
//!
//! - **File watching** (`skill-filesystem` `SkillWatchManager`, ~460 of its 1,063 lines) is genuinely
//!   absent. [`crate::PollingWatcher`] provides a generic polling snapshot and diff, but nothing
//!   skill-specific: no root relevance filter, no ancestor/root watch-mode resolution, no
//!   bounded project retention. It is an *enhancement* — without it a catalog goes stale, which is
//!   visible and recoverable, because the model re-lists on the next turn.
//! - **Workspace dependencies** (`tool-workspace-dependencies`, 322 lines) is genuinely absent too.
//!   It installs a bundled Python/Node payload for the desktop build. There is no local payload, no
//!   `runtime.json`, and no bundled interpreter, so a faithful port would be filesystem plumbing for
//!   an artifact this crate does not ship.
//! - **Invocation policy and the catalog protocol** is a *correctness* gap: it decides whether a
//!   skill may be loaded at all and what the model is told about it. It is also the widest, touching
//!   three upstream packages' public surface.
//!
//! So the choice is this third one, and the module implements it whole rather than partially: the
//! grammar, the front-matter read the policy needs, the gate, the content frame, the catalog frame,
//! its digest, and the `/name` gesture.
//!
//! # Why the front-matter reader is here
//!
//! [`crate::parse_frontmatter`] deliberately reads only `name` and `description`, and
//! documents why: a catalog scan must not pay for a YAML library. But invocation policy *is* in the
//! front matter and needs arbitrary keys, and `whenToUse` and `metadata` are dropped by that parser
//! as well. [`parse_frontmatter_block`] therefore reads the block with `serde_norway` — already a
//! dependency of this crate — and hands back the data *and* the body, which the two-field parser
//! cannot produce. Two readers with different costs for different callers, rather than one that is
//! wrong for both.
//!
//! # What is deliberately absent
//!
//! - `SkillRegistry`'s layered scope chain, collect cache, revision counter, and abort plumbing. That
//!   is the registry's own gap ([`crate::SkillRegistry`] documents its precedence model), not the policy's.
//! - `catalogHistory` and `catalogMessage`, which walk a live session's event log. They are session
//!   state, and [`digest_catalog_entries`] is the primitive they are built from.
//! - The Chokidar watcher and the desktop payload installer, per the comparison above.

use std::fmt::Write as _;

use serde_json::Value as Json;
use serde_norway::Value;

/// The public skill-name grammar, spelled out for callers that have to explain a rejection.
///
/// Upstream's `SKILL_NAME` is a module-private regex with this pattern; it is exported here because a
/// tool schema and an error message both need to quote it, and quoting a pattern from a constant beats
/// quoting it from memory.
pub const SKILL_NAME_PATTERN: &str = "^[a-z0-9]+(-[a-z0-9]+)*$";

/// Whether a string is a valid kebab-case skill name.
///
/// Upstream `isSkillName` in `packages/skill/skill/src/index.ts`. Both the registry's lookup and the
/// `skill` tool's argument validation call it, so the grammar is spelled once here.
///
/// Hand-written rather than compiled from [`SKILL_NAME_PATTERN`] because the crate has no
/// regular-expression dependency and this grammar is a loop: a run of `[a-z0-9]`, then zero or more `-`
/// followed by another run. A leading, trailing, or doubled hyphen therefore fails without a special
/// case, which is exactly the shape upstream's regex rejects. Non-ASCII characters fail because the
/// run only advances on ASCII, so the first non-ASCII byte below `0x80` ends it and the `-` check
/// rejects it while a byte at or above `0x80` stops the run and also fails.
#[must_use]
pub fn is_skill_name(name: &str) -> bool {
    let bytes = name.as_bytes();
    if bytes.is_empty() {
        return false;
    }
    let mut index = 0;
    loop {
        let run_start = index;
        while index < bytes.len()
            && (bytes[index].is_ascii_lowercase() || bytes[index].is_ascii_digit())
        {
            index += 1;
        }
        // Empty run: either a doubled hyphen or a trailing one.
        if index == run_start {
            return false;
        }
        if index == bytes.len() {
            return true;
        }
        // The only byte that may follow a run is a hyphen, and a run must follow it.
        if bytes[index] != b'-' {
            return false;
        }
        index += 1;
    }
}

/// Which surfaces may invoke a skill.
///
/// Upstream `SkillInvocationPolicy` in `packages/skill/skill/src/index.ts`. Both switches default to
/// permissive: a skill that says nothing is available everywhere, which is what makes the front-matter
/// keys optional.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct InvocationPolicy {
    /// Whether model-facing catalogs and loaders include this skill.
    ///
    /// Upstream `modelInvocable`, set from `disable-model-invocation`.
    pub model_invocable: bool,
    /// Whether human-facing command catalogs and loaders include this skill.
    ///
    /// Upstream `userInvocable`, set from `user-invocable`.
    pub user_invocable: bool,
}

impl Default for InvocationPolicy {
    /// Upstream's omission default: `{ modelInvocable: true, userInvocable: true }`.
    fn default() -> Self {
        Self {
            model_invocable: true,
            user_invocable: true,
        }
    }
}

impl InvocationPolicy {
    /// Builds a policy from its two switches.
    #[must_use]
    pub fn new(model_invocable: bool, user_invocable: bool) -> Self {
        Self {
            model_invocable,
            user_invocable,
        }
    }

    /// Whether the model may see and load this skill.
    ///
    /// Upstream `isModelInvocable`, which reads `skill.invocation.modelInvocable`.
    #[must_use]
    pub fn is_model_invocable(self) -> bool {
        self.model_invocable
    }

    /// Whether a human-facing command may see and load this skill.
    ///
    /// Upstream `isUserInvocable`, which reads `skill.invocation.userInvocable`.
    #[must_use]
    pub fn is_user_invocable(self) -> bool {
        self.user_invocable
    }
}

/// Whether the model may see and load a skill.
///
/// Upstream `isModelInvocable` in `packages/skill/skill/src/index.ts`, kept as a free function
/// because that is how it is exported and because it reads well at a filter boundary:
/// `skills.iter().filter(|skill| is_model_invocable(skill.invocation))`.
#[must_use]
pub fn is_model_invocable(policy: InvocationPolicy) -> bool {
    policy.is_model_invocable()
}

/// Whether a human-facing command may see and load a skill.
///
/// Upstream `isUserInvocable` in `packages/skill/skill/src/index.ts`.
#[must_use]
pub fn is_user_invocable(policy: InvocationPolicy) -> bool {
    policy.is_user_invocable()
}

/// Why an invocation policy could not be read from front matter.
///
/// Upstream throws a plain `Error` for a legacy key and a `TypeError` for a non-boolean; both are
/// modelled here so a caller can report the file and the field instead of a bare message.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum PolicyError {
    /// A renamed key from an older front-matter dialect is present.
    ///
    /// Upstream `rejectLegacyInvocationKey`: accepted keys were renamed, and silently honoring the
    /// old spelling would leave the new one looking authoritative when it is not.
    #[error("frontmatter field \"{field}\" is unsupported; use \"{canonical}\"")]
    Legacy {
        /// The field that was found.
        field: String,
        /// The field that should have been written.
        canonical: &'static str,
    },

    /// The field is present with a value that is not a boolean.
    ///
    /// Upstream `frontmatterBoolean`'s `TypeError`.
    #[error("frontmatter field \"{field}\" must be a boolean")]
    NotBoolean {
        /// The field that could not be read.
        field: String,
    },
}

/// A skill file's parsed front matter: the data, and the body that follows it.
///
/// Upstream `parseFrontmatter` in `packages/skill/skill-filesystem/src/index.ts` returns
/// `{ data, body }`. The body is returned un-trimmed exactly as upstream does; its one caller trims it
/// when it builds the definition.
#[derive(Clone, Debug, PartialEq)]
pub struct Frontmatter {
    /// The YAML mapping between the fences.
    pub data: Value,
    /// Everything after the closing fence line, un-trimmed.
    pub body: String,
}

/// Reads the `---` front-matter block and the body that follows it.
///
/// Upstream `parseFrontmatter` in `packages/skill/skill-filesystem/src/index.ts`. Returns `None` when
/// there is no complete block, when the block is not a YAML mapping, or when the file is a single line
/// with no newline at all (upstream's `firstLineEnd < 0`).
///
/// The fence must be the very first line, because a file that begins with prose has no front matter
/// rather than front matter further down. Trailing `\r` is tolerated on each line so a CRLF checkout
/// parses the same as an LF one; upstream does the same with `replace(/\r$/, '')` per line.
///
/// The mapping requirement is what rejects the shapes upstream rejects: a block whose YAML is a list,
/// a scalar, or empty (YAML null) is not a skill file's front matter.
#[must_use]
pub fn parse_frontmatter_block(text: &str) -> Option<Frontmatter> {
    let first_line_end = text.find('\n')?;
    if text.get(..first_line_end)?.trim_end_matches('\r') != "---" {
        return None;
    }
    let data_start = first_line_end + 1;
    let mut line_start = data_start;
    while line_start <= text.len() {
        let next_newline = text.get(line_start..).and_then(|rest| rest.find('\n'));
        let (line_end, next) = match next_newline {
            Some(offset) => (line_start + offset, Some(line_start + offset + 1)),
            None => (text.len(), None),
        };
        let line = text.get(line_start..line_end)?.trim_end_matches('\r');
        if line == "---" {
            let body_start = next.unwrap_or(text.len());
            let yaml = text.get(data_start..line_start)?;
            let data: Value = serde_norway::from_str(yaml).ok()?;
            if !data.is_mapping() {
                return None;
            }
            return Some(Frontmatter {
                data,
                body: text.get(body_start..)?.to_owned(),
            });
        }
        let next_start = next?;
        line_start = next_start;
    }
    None
}

/// A non-empty string field from parsed front matter.
///
/// Upstream `stringField` in `packages/skill/skill-filesystem/src/index.ts`: `name` and `description`
/// both go through it, which is why an empty value is treated as absent rather than as an empty
/// skill field. Returning a borrow rather than an owned copy keeps the caller from paying for two
/// strings it is about to clone anyway.
#[must_use]
pub fn frontmatter_string<'a>(data: &'a Value, key: &str) -> Option<&'a str> {
    data.get(key)
        .and_then(Value::as_str)
        .filter(|value| !value.is_empty())
}

/// An optional string field from parsed front matter.
///
/// Upstream `optionalString`, used for `whenToUse`. Same emptiness rule as [`frontmatter_string`];
/// separated because its caller spreads the field into an object conditionally rather than requiring
/// it.
#[must_use]
pub fn frontmatter_optional_string<'a>(data: &'a Value, key: &str) -> Option<&'a str> {
    frontmatter_string(data, key)
}

/// The `metadata` mapping from parsed front matter, when it is one.
///
/// Upstream `optionalMetadata`: an object that is not an array, and nothing else. A list or a scalar
/// under `metadata:` is ignored rather than reported, because it is provider-specific payload this
/// layer never interprets.
#[must_use]
pub fn frontmatter_metadata(data: &Value) -> Option<&serde_norway::Mapping> {
    data.get("metadata").and_then(Value::as_mapping)
}

/// Reads one front-matter boolean in upstream's accepted spellings.
///
/// Upstream `frontmatterBoolean` in `packages/skill/skill-filesystem/src/index.ts`. The YAML reader
/// already turns `true`/`false` into a boolean, but a quoted `"yes"` or a bare `1` is common in
/// hand-written front matter, and upstream accepts both — so a file that says `user-invocable: no`
/// must not be read as "absent, therefore permissive".
///
/// Returns `Ok(None)` when the key is absent (the caller applies its own default), `Ok(Some(_))` for a
/// recognized value, and [`PolicyError::NotBoolean`] for a present but unrecognized one. Numbers other
/// than integer `0`/`1` are rejected: upstream compares with `=== 1` and `=== 0`, so `2` is a
/// misspelling of intent rather than "truthy".
pub fn frontmatter_boolean(data: &Value, key: &str) -> Result<Option<bool>, PolicyError> {
    let Some(value) = data.get(key) else {
        return Ok(None);
    };
    match value {
        Value::Bool(flag) => Ok(Some(*flag)),
        Value::Number(number) => match number.as_i64() {
            Some(1) => Ok(Some(true)),
            Some(0) => Ok(Some(false)),
            _ => Err(PolicyError::NotBoolean {
                field: key.to_owned(),
            }),
        },
        Value::String(text) => match text.to_ascii_lowercase().as_str() {
            "1" | "true" | "yes" | "on" => Ok(Some(true)),
            "0" | "false" | "no" | "off" => Ok(Some(false)),
            _ => Err(PolicyError::NotBoolean {
                field: key.to_owned(),
            }),
        },
        _ => Err(PolicyError::NotBoolean {
            field: key.to_owned(),
        }),
    }
}

/// Reads the invocation policy out of parsed front matter.
///
/// Upstream `parseInvocationPolicy` in `packages/skill/skill-filesystem/src/index.ts`. The two keys are
/// deliberately asymmetric: `disable-model-invocation: true` turns the model surface *off*, while
/// `user-invocable: false` turns the user surface off. Both are permissive by default, so an absent key
/// never removes an ability.
///
/// The three legacy spellings are refused rather than ignored. Upstream renamed the fields because the
/// old names inverted the sense — `modelInvocable: false` and `disable-model-invocation: true` mean the
/// same thing — and honoring both would make a file that mixes them mean whichever was read last.
///
/// Only a present-but-unreadable value fails. Absent keys and a non-mapping `data` yield the defaults,
/// which is what makes this total for a parsed file.
pub fn parse_invocation_policy(data: &Value) -> Result<InvocationPolicy, PolicyError> {
    reject_legacy(data, "disableModelInvocation", "disable-model-invocation")?;
    reject_legacy(data, "modelInvocable", "disable-model-invocation")?;
    reject_legacy(data, "userInvocable", "user-invocable")?;
    // `disable-model-invocation` is inverted at this boundary: the front matter spells the switch as
    // a prohibition, and the policy as a capability, so only one of them can read naturally.
    let disable_model_invocation = frontmatter_boolean(data, "disable-model-invocation")?;
    let user_invocable = frontmatter_boolean(data, "user-invocable")?;
    Ok(InvocationPolicy {
        model_invocable: disable_model_invocation != Some(true),
        user_invocable: user_invocable != Some(false),
    })
}

/// Rejects one renamed invocation key.
///
/// Upstream `rejectLegacyInvocationKey`. Presence is what matters, not the value: a file carrying
/// `modelInvocable:` with no value is still a file written against the old dialect.
fn reject_legacy(data: &Value, legacy: &str, canonical: &'static str) -> Result<(), PolicyError> {
    if data.get(legacy).is_some() {
        return Err(PolicyError::Legacy {
            field: legacy.to_owned(),
            canonical,
        });
    }
    Ok(())
}

/// Escapes prose embedded inside skill markup.
///
/// Upstream `escapeText` in `packages/skill/skill/src/index.ts`: `&`, `<`, and `>` only, with `&` first
/// so it cannot double-escape the entities it just produced. `"` is deliberately left alone — this
/// function's output goes in text position, where a quote is not structural.
#[must_use]
pub fn escape_text(value: &str) -> String {
    value
        .replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
}

/// Escapes a value placed inside a double-quoted attribute.
///
/// Upstream `escapeAttr` in `packages/skill/skill/src/index.ts`: `&`, `"`, and `<`. `>` is not escaped
/// because it needs no escape inside an attribute value, and leaving it readable keeps the attribute
/// diffable against what the author wrote.
#[must_use]
pub fn escape_attr(value: &str) -> String {
    value
        .replace('&', "&amp;")
        .replace('"', "&quot;")
        .replace('<', "&lt;")
}

/// Where a loaded skill's relative resources resolve from.
///
/// Upstream `SkillResourceBase` in `packages/skill/skill/src/index.ts`. The three cases exist because a
/// provider can own a directory, a remote bundle, or something it would rather describe than address.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ResourceBase {
    /// A directory on the host.
    Directory {
        /// Absolute path to the skill's directory.
        path: String,
    },
    /// A base URL for a remote bundle.
    Url {
        /// Absolute URL.
        url: String,
    },
    /// A provider-managed location described in prose.
    Opaque {
        /// What the provider wants the model to know.
        description: String,
    },
}

/// The fields [`render_skill_content`] reads from a loaded skill.
///
/// Upstream's parameter type is `Pick<SkillDefinition, 'name' | 'provider' | 'resourceBase' |
/// 'content'>`, a projection rather than the whole definition — the body is all that is rendered, so a
/// caller that has only a body does not have to invent a summary to render it.
#[derive(Clone, Copy, Debug)]
pub struct SkillContentView<'a> {
    /// The skill's name, escaped into the frame's attribute.
    pub name: &'a str,
    /// The provider that supplied it, named when no resource base is known.
    pub provider: &'a str,
    /// Where its relative resources resolve from.
    pub resource_base: Option<&'a ResourceBase>,
    /// The instruction body, embedded verbatim.
    pub content: &'a str,
}

/// Renders one loaded skill as the canonical model-facing block.
///
/// Upstream `renderSkillContent` in `packages/skill/skill/src/index.ts`. Both paths that hand a skill
/// body to the model — the `skill` tool's result and the user-explicit invocation injection — call
/// this, so the model sees one shape and cannot tell which path delivered it. Keeping a second
/// renderer for the injection path is what would let the two drift.
///
/// The name is escaped as an attribute and the provider, path, URL, or description as prose. The body
/// is embedded verbatim: a skill is trusted local content the author wrote, and escaping it would put
/// `&lt;` in front of code the model is meant to copy. User-supplied text never reaches the body —
/// the invocation path passes the user's own words as a separate message.
#[must_use]
pub fn render_skill_content(skill: &SkillContentView<'_>) -> String {
    let mut lines: Vec<String> = vec![
        format!("<skill_content name=\"{}\">", escape_attr(skill.name)),
        String::from("<skill_resources>"),
    ];
    lines.extend(resource_hint(skill.provider, skill.resource_base));
    lines.push(String::from("</skill_resources>"));
    lines.push(String::new());
    lines.push(String::from("<skill_instructions>"));
    lines.push(skill.content.to_owned());
    lines.push(String::from("</skill_instructions>"));
    lines.push(String::from("</skill_content>"));
    lines.join("\n")
}

/// The two resource lines for one skill.
///
/// Upstream `renderResourceHint`. Every case names the base and then gives the same instruction — resolve
/// relative references against it, and load what is needed rather than everything — except that a
/// directory and a URL resolve differently, so the sentence differs in exactly one word.
fn resource_hint(provider: &str, base: Option<&ResourceBase>) -> Vec<String> {
    match base {
        None => vec![
            format!(
                "Resources for this skill are managed by provider \"{}\".",
                escape_text(provider)
            ),
            String::from("Load referenced resources only as needed."),
        ],
        Some(ResourceBase::Directory { path }) => vec![
            format!("Base directory for this skill: {}", escape_text(path)),
            String::from(
                "Resolve relative paths mentioned by this skill against the base directory before \
                 using them. Load referenced resources only as needed.",
            ),
        ],
        Some(ResourceBase::Url { url }) => vec![
            format!("Base URL for this skill: {}", escape_text(url)),
            String::from(
                "Resolve relative URLs mentioned by this skill against the base URL before using \
                 them. Load referenced resources only as needed.",
            ),
        ],
        Some(ResourceBase::Opaque { description }) => vec![
            format!("Resources for this skill: {}", escape_text(description)),
            String::from("Load referenced resources only as needed."),
        ],
    }
}

/// The longest description the session catalog renders, upstream's default.
///
/// Upstream `DEFAULT_CATALOG_DESCRIPTION_MAX_LENGTH` in `packages/skill/tool-skill/src/index.ts`.
pub const DEFAULT_CATALOG_DESCRIPTION_MAX_LENGTH: usize = 500;

/// The smallest accepted description limit, upstream's `assertPositiveInteger(..., 3)` floor.
///
/// Upstream `tool-skill` refuses a smaller value at configuration time; the floor exists because
/// truncation appends `...`, which is longer than the whole budget below it.
pub const MIN_CATALOG_DESCRIPTION_MAX_LENGTH: usize = 3;

/// One catalog line's durable record.
///
/// Upstream `SkillCatalogSource['entries'][number]` in `packages/skill/tool-skill/src/index.ts`: the
/// name and the *already truncated* description, recorded beside the model-facing prose so a consumer
/// presenting the list does not have to re-parse the `<available_skills>` block, whose framing exists
/// for the model.
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct CatalogEntry {
    /// The skill's name, which the grammar already guarantees needs no escaping.
    pub name: String,
    /// The description as rendered, with whitespace collapsed and any truncation applied.
    pub description: String,
}

/// One skill as the catalog sees it: the two summary lines plus its policy.
///
/// Not upstream `SkillSummary` — this is the projection of it that the catalog filter and renderer
/// need. Borrowing the strings keeps building a catalog for a hundred skills allocation-free until the
/// truncation actually copies a description.
#[derive(Clone, Copy, Debug)]
pub struct SummaryView<'a> {
    /// The skill's name.
    pub name: &'a str,
    /// Its full, untruncated description.
    pub description: &'a str,
    /// Whether each surface may invoke it.
    pub invocation: InvocationPolicy,
}

impl<'a> SummaryView<'a> {
    /// A summary available on both surfaces, which is what a skill that says nothing gets.
    #[must_use]
    pub fn new(name: &'a str, description: &'a str) -> Self {
        Self {
            name,
            description,
            invocation: InvocationPolicy::default(),
        }
    }

    /// The same summary under a narrower policy.
    #[must_use]
    pub fn with_invocation(mut self, invocation: InvocationPolicy) -> Self {
        self.invocation = invocation;
        self
    }
}

/// Normalizes and truncates one description for the catalog line.
///
/// Upstream `catalogDescription` in `packages/skill/tool-skill/src/index.ts`. Whitespace runs collapse to
/// one space and the ends are trimmed, because a description is written as YAML that may be folded over
/// several lines and the catalog is one line per skill. A description over the limit keeps its head and
/// gains `...`.
///
/// The limit counts UTF-16 units, which is what JavaScript's `String.length` reports and therefore what
/// upstream's `slice` counts. Counting characters instead would let a description of emoji run to twice
/// the configured budget in the units the client displays.
///
/// One deliberate difference from upstream: a truncation never splits a surrogate pair. JavaScript's
/// `slice` can cut one in half and leave a lone surrogate, which is not a character and renders as a
/// replacement glyph; this function drops the straddling character instead, so the result may be one
/// unit shorter than upstream's and is always well-formed.
#[must_use]
pub fn catalog_description(value: &str, max_length: usize) -> String {
    let limit = max_length.max(MIN_CATALOG_DESCRIPTION_MAX_LENGTH);
    let normalized = collapse_whitespace(value);
    if utf16_len(&normalized) <= limit {
        return normalized;
    }
    // `limit >= MIN_CATALOG_DESCRIPTION_MAX_LENGTH`, so the subtraction cannot underflow.
    let head = utf16_prefix(&normalized, limit - 3);
    format!("{head}...")
}

/// Collapses every whitespace run to one space and trims the ends.
///
/// Upstream's `value.replaceAll(/\s+/g, ' ').trim()`. JavaScript's `\s` and Rust's `char::is_whitespace`
/// agree except that `\s` also matches the byte-order mark, which [`is_js_whitespace`] adds so a
/// description copied out of a marked file normalizes the same way.
fn collapse_whitespace(value: &str) -> String {
    let mut out = String::with_capacity(value.len());
    let mut pending_space = false;
    for character in value.chars() {
        if is_js_whitespace(character) {
            pending_space = !out.is_empty();
            continue;
        }
        if pending_space {
            out.push(' ');
            pending_space = false;
        }
        out.push(character);
    }
    out
}

/// Whether a character is whitespace in JavaScript's `\s` sense.
///
/// `char::is_whitespace` covers the Unicode `White_Space` property, which is `\s` minus U+FEFF.
fn is_js_whitespace(character: char) -> bool {
    character.is_whitespace() || character == '\u{feff}'
}

/// A string's length in UTF-16 code units.
///
/// Upstream counts JavaScript's `String.length`. Duplicated here rather than imported from
/// [`crate::utf16_len`] because an included-by-path module cannot reach the crate root, and one
/// one-line `map` is cheaper than the indirection.
fn utf16_len(value: &str) -> usize {
    value.chars().map(char::len_utf16).sum()
}

/// The longest char-aligned prefix no longer than `max_units` UTF-16 units.
fn utf16_prefix(value: &str, max_units: usize) -> &str {
    let mut units = 0;
    for (offset, character) in value.char_indices() {
        let next = units + character.len_utf16();
        if next > max_units {
            return value.get(..offset).unwrap_or(value);
        }
        units = next;
    }
    value
}

/// The durable entries of one catalog publication, model-invocable skills only.
///
/// Upstream `catalogSourceEntries` in `packages/skill/tool-skill/src/index.ts`, which is called on
/// `snapshot.skills.filter(isModelInvocable)`. The filter lives here rather than at the call site so a
/// publisher cannot record an entry it did not render — the two must be the same list, because the
/// recorded entries are what a later turn compares to decide whether anything changed.
///
/// Order is preserved: upstream renders a sorted snapshot, and re-sorting here would let a caller's
/// ordering disagree with the prose it accompanied.
#[must_use]
pub fn catalog_entries(skills: &[SummaryView<'_>], max_length: usize) -> Vec<CatalogEntry> {
    skills
        .iter()
        .filter(|skill| skill.invocation.is_model_invocable())
        .map(|skill| CatalogEntry {
            name: skill.name.to_owned(),
            description: catalog_description(skill.description, max_length),
        })
        .collect()
}

/// The model-facing lines of a catalog block.
///
/// Upstream `renderCatalogEntries` in `packages/skill/tool-skill/src/index.ts`. The name rides in
/// backticks and needs no escaping because [`is_skill_name`] has already rejected anything with a
/// markup character; the description is escaped because it is free prose. The escaping belongs to the
/// rendering, not to the recorded entry, which is why [`CatalogEntry`] stores the unescaped text.
#[must_use]
pub fn render_catalog_entries(entries: &[CatalogEntry]) -> Vec<String> {
    entries
        .iter()
        .map(|entry| format!("- `{}`: {}", entry.name, escape_text(&entry.description)))
        .collect()
}

/// The notice appended when discovery could not confirm every source.
///
/// Upstream `incompleteCatalogNotice`. A partial catalog is not an empty one: a skill may be missing
/// because its root was unreadable, so the model is told that a lower-priority definition of the same
/// name might answer, and that the check will run again.
#[must_use]
pub fn incomplete_catalog_notice() -> String {
    String::from(
        "Some skills could not be checked. Only confirmed skills are listed; a lower-priority \
         definition of the same name may be selected when a preferred source is unavailable. \
         Discovery will be retried on the next request.",
    )
}

/// The first catalog a session publishes.
///
/// Upstream `renderCatalogMessage` in `packages/skill/tool-skill/src/index.ts`. The frame matters as
/// much as the lines: `<available_skills>` is what tells a consumer where the list begins and ends, and
/// the closing guidance is what stops a model from inferring instructions from a summary. The empty and
/// incomplete cases are separate sentences rather than a missing block, because an absent block reads as
/// a rendering failure while a stated absence reads as a fact.
#[must_use]
pub fn render_catalog_message(entries: &[CatalogEntry], incomplete: bool) -> String {
    let mut lines: Vec<String> = vec![String::from("<system-reminder>")];
    lines.push(String::from(if entries.is_empty() {
        "No skills could be confirmed in this discovery attempt."
    } else {
        "A skill is a reusable set of task-specific instructions. The following skills are \
         available in this session:"
    }));
    lines.push(String::new());
    lines.push(String::from("<available_skills>"));
    lines.extend(render_catalog_entries(entries));
    lines.push(String::from("</available_skills>"));
    lines.push(String::new());
    if incomplete {
        lines.push(incomplete_catalog_notice());
        lines.push(String::new());
    }
    lines.push(String::from(if entries.is_empty() {
        "Wait for an updated catalog before calling the `skill` tool. Do not infer skill \
         instructions from an empty catalog."
    } else {
        "If the user names a skill, or the task clearly matches a skill's description, call the \
         `skill` tool with the exact skill name before taking task actions. Load all applicable \
         skills, then follow their full instructions. This catalog contains summaries only; do not \
         infer or follow a skill's instructions until it has been loaded."
    }));
    lines.push(String::from(
        "A user may also invoke a skill directly; its <skill_content> block then appears in this \
         conversation. Follow it, and do not call the `skill` tool again for that skill.",
    ));
    lines.push(String::from("</system-reminder>"));
    lines.join("\n")
}

/// A catalog that replaces every earlier one in the session.
///
/// Upstream `renderCatalogUpdate` in `packages/skill/tool-skill/src/index.ts`. The difference from
/// [`render_catalog_message`] is not decoration: a session accumulates catalogs, and a model reading a
/// bare list has no way to know whether it is an addition or the whole truth. This frame says which,
/// and the empty case says explicitly not to reuse names from an earlier catalog — the failure mode a
/// deleted skill otherwise leaves behind.
#[must_use]
pub fn render_catalog_update(entries: &[CatalogEntry], incomplete: bool) -> String {
    let availability: Vec<&str> = if entries.is_empty() {
        vec![
            if incomplete {
                "No skills could be confirmed in this discovery attempt. Do not use names from \
                 earlier skill catalogs."
            } else {
                "No skills are currently available through the `skill` tool. Do not use names from \
                 earlier skill catalogs."
            },
            "A user may still invoke a skill directly; its <skill_content> block then appears in \
             this conversation. Follow it, and do not call the `skill` tool for it.",
        ]
    } else {
        vec![
            "Use only names in this replacement catalog. If the user names a listed skill, or the \
             task clearly matches its description, call the `skill` tool with the exact name before \
             acting.",
            "A user may also invoke a skill directly; its <skill_content> block then appears in \
             this conversation. Follow it, and do not call the `skill` tool again for that skill.",
        ]
    };
    let mut lines: Vec<String> = vec![String::from("<system-reminder>")];
    lines.push(String::from(if incomplete {
        "Skill discovery is incomplete. This catalog of currently available skills replaces every \
         earlier available-skills list in this session:"
    } else {
        "The available skill catalog changed. This complete catalog replaces every earlier \
         available-skills list in this session:"
    }));
    lines.push(String::new());
    lines.push(String::from("<available_skills>"));
    lines.extend(render_catalog_entries(entries));
    lines.push(String::from("</available_skills>"));
    lines.push(String::new());
    if incomplete {
        lines.push(incomplete_catalog_notice());
        lines.push(String::new());
    }
    lines.extend(availability.into_iter().map(str::to_owned));
    lines.push(String::from("</system-reminder>"));
    lines.join("\n")
}

/// The identity of one catalog publication, as a lowercase SHA-256 hex digest.
///
/// Upstream `digestCatalogEntries` in `packages/skill/tool-skill/src/index.ts`. Two catalogs are the same
/// publication exactly when their entries and their completeness match, and the digest is what a step
/// listener compares against the last catalog it published so an unchanged list is not re-injected every
/// turn. Completeness is part of the identity because a recovery from incomplete to complete discovery
/// must clear the warning even when the entries are identical — so the flag is a prefix line, not a
/// field appended after the payload.
///
/// Each entry is encoded as a JSON array of two strings and the encodings are joined with newlines.
/// JSON rather than a separator character because every separator is itself a legal description
/// character, so only quoting makes the boundary exact — a description ending in `-` and a following
/// name could otherwise produce the same bytes as a different pair.
#[must_use]
pub fn digest_catalog_entries(entries: &[CatalogEntry], incomplete: bool) -> String {
    let mut canonical = String::new();
    for (index, entry) in entries.iter().enumerate() {
        if index > 0 {
            canonical.push('\n');
        }
        // A two-element array of strings always serializes; the `write!` failure case does not exist
        // for `String`, and the result is dropped rather than unwrapped so this stays total.
        let _ = write!(
            canonical,
            "[\"{}\",\"{}\"]",
            json_escape(&entry.name),
            json_escape(&entry.description)
        );
    }
    let mut input = String::new();
    if incomplete {
        input.push_str("incomplete\n");
    }
    input.push_str(&canonical);
    sha256_hex(input.as_bytes())
}

/// Escapes one string the way `JSON.stringify` does.
///
/// `serde_json` would do this, but the digest above is assembled as text so that the separator placement
/// stays visible beside the upstream line it mirrors. The escapes here are the ones `JSON.stringify`
/// emits: `"` and `\`, the five shorthand controls, `\u00XX` for the remaining controls below `0x20`,
/// and everything else — including non-ASCII — verbatim.
fn json_escape(value: &str) -> String {
    let mut out = String::with_capacity(value.len());
    for character in value.chars() {
        match character {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\u{8}' => out.push_str("\\b"),
            '\u{c}' => out.push_str("\\f"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            control if (control as u32) < 0x20 => {
                let _ = write!(out, "\\u{:04x}", control as u32);
            }
            other => out.push(other),
        }
    }
    out
}

/// What one session catalog message recorded beside its prose.
///
/// Upstream `SkillCatalogSource` in `packages/skill/tool-skill/src/index.ts`, minus the `kind`/`form`
/// discriminators, which belong to the message envelope rather than to this fact.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CatalogSource {
    /// Exactly the entries the message published, in catalog order.
    pub entries: Vec<CatalogEntry>,
    /// Marks a replacement catalog rather than this session's first publication.
    pub update: bool,
    /// Some discovery failed, so the entries are only what this attempt confirmed.
    ///
    /// Upstream represents this as an absent-vs-`true` field; a bool is the same fact in a language
    /// whose record always has the field.
    pub incomplete: bool,
}

/// Reads a durable catalog record, or `None` when it is not a usable catalog.
///
/// Upstream `readCatalogEntries` plus the flags its caller reads, in
/// `packages/skill/tool-skill/src/index.ts`. A resumed or forked session may contain a seed written by an
/// older version, and seed validation only guarantees a source object with a non-empty `kind` — so an
/// unreadable record is treated as "not this plugin's catalog" rather than throwing inside a step
/// listener, which would fail every subsequent turn of that session.
///
/// All-or-nothing on purpose: one malformed entry means the record cannot be trusted as a list, and a
/// partially read catalog would publish entries the original never contained.
#[must_use]
pub fn read_catalog_source(source: &Json) -> Option<CatalogSource> {
    let entries = read_catalog_entries(source)?;
    Some(CatalogSource {
        entries,
        update: source.get("update").and_then(Json::as_bool) == Some(true),
        incomplete: source.get("incomplete").and_then(Json::as_bool) == Some(true),
    })
}

/// Reads the `entries` array of a durable catalog record.
///
/// Upstream `readCatalogEntries` in `packages/skill/tool-skill/src/index.ts`. An empty name is rejected
/// along with a non-string one: a nameless entry cannot be looked up, so a record containing it is not a
/// catalog this code can use.
#[must_use]
pub fn read_catalog_entries(source: &Json) -> Option<Vec<CatalogEntry>> {
    let entries = source.get("entries")?.as_array()?;
    let mut readable = Vec::with_capacity(entries.len());
    for entry in entries {
        let name = entry.get("name")?.as_str()?;
        let description = entry.get("description")?.as_str()?;
        if name.is_empty() {
            return None;
        }
        readable.push(CatalogEntry {
            name: name.to_owned(),
            description: description.to_owned(),
        });
    }
    Some(readable)
}

/// One text block claimed for a step, and whether it came from the user.
///
/// Upstream distinguishes the sources as `message.source.kind`, and only `'user'` may carry the gesture.
/// The distinction is the whole security property: any other source — a tool result, a catalog, a remote
/// event — could otherwise contain `/deploy` and cause a skill body to be injected as if the person had
/// asked for it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ClaimedText {
    /// Whether this block is direct user input.
    pub from_user: bool,
    /// The block's text.
    pub text: String,
}

impl ClaimedText {
    /// Direct user input, which may carry a gesture.
    #[must_use]
    pub fn user(text: impl Into<String>) -> Self {
        Self {
            from_user: true,
            text: text.into(),
        }
    }

    /// Any other source, which may not.
    #[must_use]
    pub fn other(text: impl Into<String>) -> Self {
        Self {
            from_user: false,
            text: text.into(),
        }
    }
}

/// The `/name` gesture tokens in one text, in the order they appear.
///
/// Upstream `SKILL_GESTURE` in `packages/skill/tool-skill/src/index.ts`: `/(^|\s)\/(name)(?=\s|$)/g`.
///
/// Both boundaries are load-bearing. The leading one keeps file paths out, since `/usr` in `cd /usr/bin`
/// is preceded by a space but followed by `/` — which fails the trailing boundary — while `cat /etc` would
/// match, which is the accepted cost of a gesture that has to be sayable. The trailing one keeps
/// fractions and paths that start mid-token out. The name grammar is the same one [`is_skill_name`]
/// enforces, so a token this function returns is already a name the registry could hold.
///
/// This function does not deduplicate; [`invoked_skill_names`] does, across messages.
#[must_use]
pub fn skill_gestures_in_text(text: &str) -> Vec<String> {
    let characters: Vec<char> = text.chars().collect();
    let mut names: Vec<String> = Vec::new();
    // Whether the character before the current position is a boundary: start of text or whitespace.
    let mut at_boundary = true;
    let mut index = 0;
    while index < characters.len() {
        let character = characters[index];
        if character == '/' && at_boundary {
            let name_start = index + 1;
            let mut end = name_start;
            while end < characters.len() && is_name_byte(characters[end]) {
                end += 1;
            }
            // The lookahead: whitespace or end of text, and a non-empty, grammar-valid run.
            //
            // A gesture is whitespace-bounded in both directions, which is what keeps `/usr/bin` and `5/8` out. The
            // consequence is deliberate and worth naming: `/commit, /review` yields only `review`, because the comma
            // after the first name is not whitespace. A caller that wants two gestures writes them separated by space.
            let terminated = end == characters.len() || is_js_whitespace(characters[end]);
            if end > name_start && terminated {
                let name: String = characters
                    .get(name_start..end)
                    .unwrap_or_default()
                    .iter()
                    .collect();
                if is_skill_name(&name) {
                    names.push(name);
                }
            }
            index = end.max(name_start);
            // The character consumed last was part of a name run (or a hyphen-free empty run), so the
            // next position is not a boundary; recompute from the character actually behind it.
            at_boundary = index > 0
                && characters
                    .get(index - 1)
                    .is_some_and(|c| is_js_whitespace(*c));
            continue;
        }
        at_boundary = is_js_whitespace(character);
        index += 1;
    }
    names
}

/// Whether a character may appear in the name run of a gesture.
///
/// The same class as [`SKILL_NAME_PATTERN`]: lowercase ASCII alphanumeric or hyphen. Hyphens are
/// included so the run covers a whole kebab name, and [`is_skill_name`] then decides whether the run is
/// a valid name — which is what rejects `/a-` and `/a--b`.
fn is_name_byte(character: char) -> bool {
    character.is_ascii_lowercase() || character.is_ascii_digit() || character == '-'
}

/// The skill names a step's claimed messages invoke.
///
/// Upstream `invokedSkillNames` in `packages/skill/tool-skill/src/index.ts`. Only direct user input is
/// scanned and names are deduplicated in first-seen order, so a person who writes `/review` twice gets one
/// injection and a tool result that happens to contain `/review` gets none.
#[must_use]
pub fn invoked_skill_names(messages: &[ClaimedText]) -> Vec<String> {
    let mut names: Vec<String> = Vec::new();
    for message in messages.iter().filter(|message| message.from_user) {
        for name in skill_gestures_in_text(&message.text) {
            if !names.contains(&name) {
                names.push(name);
            }
        }
    }
    names
}

/// Why the `skill` tool refused a load.
///
/// Upstream `packages/skill/tool-skill/src/index.ts` throws three `Error`s from its `execute`. They are
/// modelled as variants so the message is the same wherever it is rendered and a caller can distinguish
/// "the name is not even a name" from "the skill exists but not for you".
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum SkillLoadError {
    /// The requested name does not match the skill-name grammar.
    #[error("invalid skill name \"{name}\"")]
    InvalidName {
        /// The name as requested.
        name: String,
    },

    /// No skill with that name is currently advertised.
    #[error("skill \"{name}\" is unknown or no longer available")]
    Unknown {
        /// The name as requested.
        name: String,
    },

    /// The skill exists but its author disabled model invocation.
    #[error("skill \"{name}\" is not available for model invocation")]
    NotModelInvocable {
        /// The name as requested.
        name: String,
    },
}

/// The gate a model-facing load passes through.
///
/// Upstream's `skill` tool `execute`, which checks the grammar, then that the summary exists, then that
/// the model may invoke it — and checks the policy again on the loaded definition, because a provider can
/// change what it returns between the two reads. `invocation` is that policy: `None` means the summary
/// was not found.
///
/// The order is what makes the message useful. A malformed name is reported as malformed rather than as
/// unknown, so a model that typed `/Review` is told which rule it broke instead of being sent to re-list a
/// catalog that was never going to contain it.
///
/// # Errors
///
/// Returns [`SkillLoadError`] for each of the three refusals, in the order above.
pub fn check_model_load(
    name: &str,
    invocation: Option<InvocationPolicy>,
) -> Result<(), SkillLoadError> {
    if !is_skill_name(name) {
        return Err(SkillLoadError::InvalidName {
            name: name.to_owned(),
        });
    }
    let Some(policy) = invocation else {
        return Err(SkillLoadError::Unknown {
            name: name.to_owned(),
        });
    };
    if !policy.is_model_invocable() {
        return Err(SkillLoadError::NotModelInvocable {
            name: name.to_owned(),
        });
    }
    Ok(())
}

// ---- SHA-256 -----------------------------------------------------------------------------------
//
// The catalog digest is SHA-256 because the recorded identity is what a later turn compares against,
// and a resumed session compares across process lifetimes. FIPS 180-4 is short and fully specified, so
// it is implemented here rather than adding a dependency whose only use would be this function; the
// known-answer tests in `tests/skill_gap.rs` pin it against published vectors.

/// The SHA-256 round constants: the first 32 bits of the fractional parts of the cube roots of the
/// first 64 primes. FIPS 180-4 §4.2.2.
const SHA256_K: [u32; 64] = [
    0x428a_2f98,
    0x7137_4491,
    0xb5c0_fbcf,
    0xe9b5_dba5,
    0x3956_c25b,
    0x59f1_11f1,
    0x923f_82a4,
    0xab1c_5ed5,
    0xd807_aa98,
    0x1283_5b01,
    0x2431_85be,
    0x550c_7dc3,
    0x72be_5d74,
    0x80de_b1fe,
    0x9bdc_06a7,
    0xc19b_f174,
    0xe49b_69c1,
    0xefbe_4786,
    0x0fc1_9dc6,
    0x240c_a1cc,
    0x2de9_2c6f,
    0x4a74_84aa,
    0x5cb0_a9dc,
    0x76f9_88da,
    0x983e_5152,
    0xa831_c66d,
    0xb003_27c8,
    0xbf59_7fc7,
    0xc6e0_0bf3,
    0xd5a7_9147,
    0x06ca_6351,
    0x1429_2967,
    0x27b7_0a85,
    0x2e1b_2138,
    0x4d2c_6dfc,
    0x5338_0d13,
    0x650a_7354,
    0x766a_0abb,
    0x81c2_c92e,
    0x9272_2c85,
    0xa2bf_e8a1,
    0xa81a_664b,
    0xc24b_8b70,
    0xc76c_51a3,
    0xd192_e819,
    0xd699_0624,
    0xf40e_3585,
    0x106a_a070,
    0x19a4_c116,
    0x1e37_6c08,
    0x2748_774c,
    0x34b0_bcb5,
    0x391c_0cb3,
    0x4ed8_aa4a,
    0x5b9c_ca4f,
    0x682e_6ff3,
    0x748f_82ee,
    0x78a5_636f,
    0x84c8_7814,
    0x8cc7_0208,
    0x90be_fffa,
    0xa450_6ceb,
    0xbef9_a3f7,
    0xc671_78f2,
];

/// The SHA-256 initial hash value. FIPS 180-4 §5.3.3.
const SHA256_H: [u32; 8] = [
    0x6a09_e667,
    0xbb67_ae85,
    0x3c6e_f372,
    0xa54f_f53a,
    0x510e_527f,
    0x9b05_688c,
    0x1f83_d9ab,
    0x5be0_cd19,
];

/// The lowercase hex SHA-256 of `data`.
///
/// The round variables are named `a`–`h` because those are the names FIPS 180-4 §6.2.2 uses for them,
/// and a renamed set would hide a transcription error in the one place where a transcription error is
/// invisible until a digest is compared against a published vector. This is the same exemption
/// [`crate::host_gap::sha1`] takes, for the same reason.
#[allow(
    clippy::many_single_char_names,
    reason = "the round variable names are the ones FIPS 180-4 uses; renaming them would hide a transcription error"
)]
fn sha256_hex(data: &[u8]) -> String {
    let mut state = SHA256_H;
    // Padding: the message, a `1` bit, zero bits to a 56-byte boundary, and the length in bits as a
    // big-endian 64-bit number.
    let bit_length = (data.len() as u64).wrapping_mul(8);
    let mut padded = Vec::with_capacity(data.len() + 72);
    padded.extend_from_slice(data);
    padded.push(0x80);
    while padded.len() % 64 != 56 {
        padded.push(0);
    }
    padded.extend_from_slice(&bit_length.to_be_bytes());

    let mut words = [0_u32; 64];
    for chunk in padded.chunks_exact(64) {
        // The first sixteen words are the chunk's eight-byte big-endian words.
        for (word, group) in words.iter_mut().zip(chunk.chunks_exact(4)) {
            *word = u32::from_be_bytes([group[0], group[1], group[2], group[3]]);
        }
        // The rest are derived: W[t] = s1(W[t-2]) + W[t-7] + s0(W[t-15]) + W[t-16].
        for round in 16..64 {
            let (head, tail) = words.split_at_mut(round);
            let w15 = head[round - 15];
            let w2 = head[round - 2];
            let w7 = head[round - 7];
            let w16 = head[round - 16];
            let s0 = w15.rotate_right(7) ^ w15.rotate_right(18) ^ (w15 >> 3);
            let s1 = w2.rotate_right(17) ^ w2.rotate_right(19) ^ (w2 >> 10);
            if let Some(slot) = tail.first_mut() {
                *slot = w16.wrapping_add(s0).wrapping_add(w7).wrapping_add(s1);
            }
        }

        let mut a = state[0];
        let mut b = state[1];
        let mut c = state[2];
        let mut d = state[3];
        let mut e = state[4];
        let mut f = state[5];
        let mut g = state[6];
        let mut h = state[7];
        for (&constant, &word) in SHA256_K.iter().zip(words.iter()) {
            let s1 = e.rotate_right(6) ^ e.rotate_right(11) ^ e.rotate_right(25);
            let choose = (e & f) ^ ((!e) & g);
            let temp1 = h
                .wrapping_add(s1)
                .wrapping_add(choose)
                .wrapping_add(constant)
                .wrapping_add(word);
            let s0 = a.rotate_right(2) ^ a.rotate_right(13) ^ a.rotate_right(22);
            let majority = (a & b) ^ (a & c) ^ (b & c);
            let temp2 = s0.wrapping_add(majority);
            h = g;
            g = f;
            f = e;
            e = d.wrapping_add(temp1);
            d = c;
            c = b;
            b = a;
            a = temp1.wrapping_add(temp2);
        }
        state[0] = state[0].wrapping_add(a);
        state[1] = state[1].wrapping_add(b);
        state[2] = state[2].wrapping_add(c);
        state[3] = state[3].wrapping_add(d);
        state[4] = state[4].wrapping_add(e);
        state[5] = state[5].wrapping_add(f);
        state[6] = state[6].wrapping_add(g);
        state[7] = state[7].wrapping_add(h);
    }
    hex(&state)
}

/// Lowercase hexadecimal for a sequence of words, most significant byte first.
fn hex(words: &[u32]) -> String {
    const DIGITS: &[u8; 16] = b"0123456789abcdef";
    let mut out = String::with_capacity(words.len() * 8);
    for &word in words {
        for byte in word.to_be_bytes() {
            out.push(char::from(DIGITS[usize::from(byte >> 4)]));
            out.push(char::from(DIGITS[usize::from(byte & 0x0f)]));
        }
    }
    out
}
