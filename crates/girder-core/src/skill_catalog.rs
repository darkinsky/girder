//! Skill discovery: which skill files exist under a set of roots, what they are called, and how that
//! list reads in a prompt.
//!
//! A skill on disk is either a directory bundle (`<root>/<dir>/SKILL.md`) or a flat Markdown file
//! (`<root>/<name>.md`). In both shapes the same front matter carries the name and the description,
//! and the body carries the procedure. This module only reads the front matter: the body is what a
//! loader reads after the model has already chosen, and reading it here would make a catalog scan pay
//! for every skill's instructions.
//!
//! # Why discovery is one level deep
//!
//! A root is a shelf of skills, not a tree: `<root>/<name>/SKILL.md` and `<root>/<name>.md` are the
//! only two shapes upstream accepts, and a recursive walk would turn a nested `references/` directory
//! into a phantom skill. Keeping the walk shallow also keeps the cost of a scan proportional to the
//! number of skills instead of the number of files in a bundle.
//!
//! # Why a hand-written front-matter reader
//!
//! A YAML library would be the single heaviest dependency in this crate, and a catalog needs exactly
//! two scalar fields. [`parse_frontmatter`] documents the subset it accepts — enough for the files
//! people actually write, and explicit about what it silently ignores.
//!
//! # Why failures are skipped rather than reported
//!
//! [`discover`] returns a plain list: a project with no skills directory is normal, and one unreadable
//! or malformed skill file must not hide the others. The alternative — a `Result` that aborts the scan
//! — would let a single bad file in `~/.agents/skills` empty the whole catalog. A caller that wants to
//! report the casualties can parse each file itself with [`parse_frontmatter`].

use std::collections::HashSet;
use std::fs;
use std::path::PathBuf;

/// The file name of a directory-bundle skill.
const SKILL_FILE: &str = "SKILL.md";

/// One skill's metadata, read from its `SKILL.md` front matter.
///
/// The description is what a model reads when deciding whether a skill applies; [`SkillMeta::path`] is
/// where the body and the skill's resource files live. Loaders use the path's parent directory as the
/// base for relative references in the body.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SkillMeta {
    /// The name a consumer invokes.
    pub name: String,
    /// One line, shown in the catalog.
    pub description: String,
    /// The skill file itself — `SKILL.md` for a bundle, `<name>.md` for a flat skill.
    ///
    /// This is the file, not its directory: the directory is the resource base and follows from
    /// `path.parent()`, while the reverse does not hold for a flat skill, whose resources are shared
    /// with its sibling skills.
    pub path: PathBuf,
}

/// Scans each root for skills, in the order the roots are given.
///
/// # Order and duplicates
///
/// Roots are visited in the caller's order and their entries in byte order of the file name, so the
/// result is deterministic across runs and platforms. The first skill to claim a name keeps it and a
/// later one is dropped: the caller passes the most specific root first (a project's `.dsh/skills`
/// before the user's `~/.agents/skills`), which makes "first wins" the same precedence upstream gets
/// from ranking roots. Keeping both copies would leave two entries with one name in the catalog and
/// make a lookup ambiguous for no gain.
///
/// # What is skipped, and why
///
/// A missing root, an unreadable directory, a non-UTF-8 or unreadable file, a file without front
/// matter, and a file whose front matter lacks a name or a description are all skipped silently (the
/// module documentation says why). A directory entry whose name starts with `.` is skipped too: a
/// dot-directory in a skills shelf is bookkeeping (`.system`, `.git`), not a skill.
///
/// Symlinks are followed, so a shared skill directory can be linked into a project root.
#[must_use]
pub fn discover(roots: &[PathBuf]) -> Vec<SkillMeta> {
    let mut skills: Vec<SkillMeta> = Vec::new();
    let mut claimed: HashSet<String> = HashSet::new();
    for root in roots {
        let Ok(entries) = fs::read_dir(root) else {
            continue;
        };
        let mut paths: Vec<PathBuf> = entries
            .filter_map(|entry| entry.ok().map(|entry| entry.path()))
            .collect();
        // Sorted before any parse so that a duplicate name is resolved the same way on every run and
        // on every filesystem.
        paths.sort();
        for path in paths {
            let file_name = path
                .file_name()
                .map(|name| name.to_string_lossy().into_owned())
                .unwrap_or_default();
            if file_name.starts_with('.') {
                continue;
            }
            // `fs::metadata` follows symlinks; `DirEntry::metadata` would not, and a linked skill
            // directory is a normal way to share one bundle between projects.
            let Ok(metadata) = fs::metadata(&path) else {
                continue;
            };
            let file = if metadata.is_dir() {
                path.join(SKILL_FILE)
            } else if metadata.is_file() && is_markdown(&path) {
                path
            } else {
                continue;
            };
            let Ok(text) = fs::read_to_string(&file) else {
                continue;
            };
            let Some((name, description)) = parse_frontmatter(&text) else {
                continue;
            };
            if !claimed.insert(name.clone()) {
                continue;
            }
            skills.push(SkillMeta {
                name,
                description,
                path: file,
            });
        }
    }
    skills
}

/// Reads `name` and `description` out of a skill file's front matter.
///
/// Returns `None` when the file has no front matter, when the front matter is not terminated, or
/// when either field is absent or blank after trimming.
///
/// # The supported subset
///
/// This is deliberately not YAML. Each item below is the whole rule, not a summary of one:
///
/// - The front matter must start at the first byte of the file, after an optional UTF-8 BOM, with a
///   line that is exactly `---`. The closing line is the next line that is exactly `---`. A file
///   that ends before the closing fence has no front matter.
/// - Only unindented `key: value` lines are read. A key is ASCII alphanumeric, `-`, or `_`. Nested
///   maps, lists, anchors, tags, and multi-document files are ignored, not rejected — the two fields
///   read here never need them.
/// - Only `name` and `description` are extracted; every other key is ignored. The first occurrence of
///   a key wins, so a later line that happens to look like a key cannot silently rename a skill.
/// - A value is either inline (`name: demo`), folded from the lines that follow it (each indented or
///   blank, joined with single spaces), a block scalar introduced by `>` or `|` (with an optional
///   `-`/`+` chomping indicator), or quoted with `"` or `'`.
/// - In a double-quoted value, `\\`, `\"`, `\t` and `\n` are decoded; any other backslash is kept
///   literally, and an unterminated quote drops the key. In a single-quoted value, `''` is one quote.
///   A quoted value ends at its closing quote, so following indented lines are not appended to it.
/// - In an unquoted (plain) or block value, ` #` starts a comment and is dropped, along with what
///   follows; a `#` not preceded by whitespace stays (so `https://host/#anchor` survives).
/// - Line endings may be `\n` or `\r\n`.
///
/// # Why multi-line values are folded to one line
///
/// A description is a catalog line; a newline inside it would break the one-line-per-skill rendering
/// in [`render_list`]. Rather than teach every consumer to flatten it, the parser folds continuation
/// lines, and folds `|` literal blocks too — a literal block's newlines are meaningless for the only
/// two fields read here.
///
/// # Why the name is not validated against upstream's charset
///
/// Upstream requires `^[a-z0-9]+(-[a-z0-9]+)*$` and drops the file when it fails. This parser only
/// requires a non-blank name, because rejecting here would delete a real skill from the catalog over
/// a formatting choice, and the party that owns the rule (the registry, which must match a lookup by
/// name) can apply it while holding the parsed value. [`render_list`] escapes and flattens whatever
/// name it is given, so a lax name cannot break the catalog's shape either.
#[must_use]
pub fn parse_frontmatter(text: &str) -> Option<(String, String)> {
    let text = text.strip_prefix('\u{feff}').unwrap_or(text);
    let mut lines = text.lines();
    if !lines.next().is_some_and(is_fence) {
        return None;
    }
    let mut body: Vec<&str> = Vec::new();
    let mut terminated = false;
    for line in lines {
        if is_fence(line) {
            terminated = true;
            break;
        }
        body.push(line);
    }
    if !terminated {
        return None;
    }

    let mut name: Option<String> = None;
    let mut description: Option<String> = None;
    let mut index = 0;
    while index < body.len() {
        let line = body[index];
        index += 1;
        let Some((key, inline)) = split_key(line) else {
            continue;
        };
        let Some(value) = read_value(inline, &body, &mut index) else {
            continue;
        };
        let value = value.trim();
        if value.is_empty() {
            continue;
        }
        match key {
            "name" if name.is_none() => name = Some(value.to_owned()),
            "description" if description.is_none() => description = Some(value.to_owned()),
            _ => {}
        }
    }

    let name = name?;
    let description = description?;
    Some((name, description))
}

/// Renders the catalog as prompt text: a count line, then one line per skill, sorted by name.
///
/// The shape is fixed because a prompt is compared against itself across turns; a reordering that no
/// one intended would invalidate the model's cached prefix. Sorting also means the caller's slice
/// order — which [`discover`] uses for precedence — cannot leak into the prompt.
///
/// ```text
/// 2 skills available
/// - `demo`: does something
/// - `other`: does something else
/// ```
///
/// The count is omitted from the skill lines and always present, because a model reading a truncated
/// list needs one number to tell whether it saw all of them. An empty slice renders a single sentence
/// rather than an empty string: a prompt section that vanished would read as a rendering bug.
///
/// Names and descriptions are flattened to one line and `&`, `<`, `>` are escaped, so a description
/// cannot close the frame the catalog is placed in or inject markup. This function does not dedupe;
/// [`discover`] already guarantees one entry per name, and a caller that assembles a list by hand is
/// responsible for its own.
#[must_use]
pub fn render_list(skills: &[SkillMeta]) -> String {
    if skills.is_empty() {
        return String::from("no skills are available");
    }
    let mut ordered: Vec<&SkillMeta> = skills.iter().collect();
    // Stable, so two entries that share a name keep the caller's order instead of swapping between
    // runs.
    ordered.sort_by(|left, right| left.name.cmp(&right.name));

    let mut out = format!(
        "{} skill{} available",
        ordered.len(),
        if ordered.len() == 1 { "" } else { "s" }
    );
    for skill in ordered {
        out.push_str("\n- `");
        out.push_str(&escape_prose(&one_line(&skill.name)));
        out.push_str("`: ");
        out.push_str(&escape_prose(&one_line(&skill.description)));
    }
    out
}

/// Whether a directory entry is a flat Markdown skill.
///
/// The extension is compared case-insensitively because a skill written on a case-insensitive
/// filesystem may be named `SKILL.MD`, and dropping it would make the catalog depend on which machine
/// checked the files out.
fn is_markdown(path: &std::path::Path) -> bool {
    path.extension()
        .is_some_and(|extension| extension.eq_ignore_ascii_case("md"))
}

/// Whether a line is a front-matter fence.
///
/// Trailing whitespace is tolerated because it is invisible and a hard-to-diagnose reason to ignore a
/// whole skill; leading whitespace is not, because an indented `---` inside a block scalar is content.
fn is_fence(line: &str) -> bool {
    line.trim_end() == "---"
}

/// Splits an unindented `key: value` line, rejecting anything that is not a top-level mapping entry.
///
/// A line that does not match is not an error: it may be a list item, a nested key, or prose before
/// the fence, and ignoring it is how the subset stays predictable.
fn split_key(line: &str) -> Option<(&str, &str)> {
    if line.starts_with([' ', '\t']) {
        return None;
    }
    let (key, rest) = line.split_once(':')?;
    let key = key.trim();
    let valid = !key.is_empty()
        && key.chars().all(|character| {
            character.is_ascii_alphanumeric() || character == '-' || character == '_'
        });
    valid.then_some((key, rest))
}

/// Reads one value, advancing `index` past the lines it consumed.
///
/// Returns `None` when a quoted value is unterminated or the escape sequence makes it unusable; the
/// caller drops the key, which leaves a missing name or description and therefore no skill.
fn read_value(inline: &str, body: &[&str], index: &mut usize) -> Option<String> {
    let raw = inline.trim_start();
    if let Some(rest) = raw.strip_prefix(['"', '\'']) {
        return parse_quoted(raw.chars().next()?, rest);
    }
    if raw.starts_with(['>', '|']) {
        // The block header itself carries no text: the indicator, an optional chomping marker, and
        // an optional explicit indentation number are all structural.
        return Some(fold(&collect_continuation(body, index)));
    }
    let first = strip_comment(raw);
    let mut pieces = vec![first.to_owned()];
    pieces.push(fold(&collect_continuation(body, index)));
    Some(pieces.join(" "))
}

/// Collects the indented and blank lines that belong to the value that just started.
///
/// A blank line is consumed but contributes nothing, which is what makes a trailing empty line before
/// the next key disappear instead of leaving a double space in the description.
fn collect_continuation<'a>(body: &[&'a str], index: &mut usize) -> Vec<&'a str> {
    let mut lines: Vec<&str> = Vec::new();
    while *index < body.len() {
        let line = body[*index];
        if !(line.trim().is_empty() || line.starts_with([' ', '\t'])) {
            break;
        }
        *index += 1;
        let content = line.trim();
        if !content.is_empty() {
            lines.push(content);
        }
    }
    lines
}

/// Joins folded lines with single spaces, dropping leading and trailing space.
///
/// Used for both block scalars and plain multi-line values: the only fields read here are a name and
/// a one-line description, so a newline has no meaning left to preserve.
fn fold(lines: &[&str]) -> String {
    lines
        .iter()
        .map(|line| line.trim())
        .filter(|line| !line.is_empty())
        .collect::<Vec<&str>>()
        .join(" ")
}

/// Decodes a quoted scalar that starts with `quote`.
///
/// `rest` is everything after the opening quote. Text after the closing quote is discarded, which
/// covers the two things that legitimately follow it — a ` #` comment and nothing at all.
fn parse_quoted(quote: char, rest: &str) -> Option<String> {
    let mut out = String::new();
    let mut characters = rest.chars();
    if quote == '\'' {
        while let Some(character) = characters.next() {
            if character == '\'' {
                // A doubled quote is the YAML escape for one quote; a single one ends the value.
                if characters.as_str().starts_with('\'') {
                    out.push('\'');
                    characters.next();
                } else {
                    return Some(out);
                }
            } else {
                out.push(character);
            }
        }
        return None;
    }
    while let Some(character) = characters.next() {
        match character {
            '"' => return Some(out),
            '\\' => match characters.next() {
                Some('n') => out.push('\n'),
                Some('t') => out.push('\t'),
                Some(escaped) => out.push(escaped),
                // A trailing backslash means the value was cut off; there is nothing to decode.
                None => return None,
            },
            other => out.push(other),
        }
    }
    None
}

/// Cuts a plain scalar at its trailing comment.
///
/// `#` only introduces a comment after whitespace in YAML, which is what keeps a URL fragment such as
/// `https://host/page#section` intact instead of truncating it at the `#`.
fn strip_comment(raw: &str) -> &str {
    let mut previous_was_space = false;
    for (position, character) in raw.char_indices() {
        if character == '#' && previous_was_space {
            return raw[..position].trim_end();
        }
        previous_was_space = character.is_whitespace();
    }
    raw.trim_end()
}

/// Collapses every run of whitespace to a single space, so a value occupies one line.
fn one_line(value: &str) -> String {
    value.split_whitespace().collect::<Vec<&str>>().join(" ")
}

/// Escapes the three characters that could close or open markup around model-facing prose.
///
/// `&` goes first; escaping it after `<` and `>` would double-escape the entities this function just
/// produced.
fn escape_prose(value: &str) -> String {
    value
        .replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
}
