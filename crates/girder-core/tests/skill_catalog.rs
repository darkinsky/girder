//! Tests for the skill catalog: front-matter parsing, directory discovery, and prompt rendering.
//!
//! # Why the parser cases name their input
//!
//! The subset documented on [`parse_frontmatter`] is defined by what it *ignores* as much as by what it
//! reads. A test that only checked the happy path would keep passing if a nested map, a comment, or a
//! second `name:` silently changed the answer, so every case below pins one rule from that list.
//!
//! # Why discovery runs against a real directory
//!
//! The shapes being distinguished — directory bundle, flat `.md`, dot-directory, non-Markdown file,
//! missing root — exist only in the filesystem. A mocked reader would assert the code's own idea of those
//! shapes instead of the filesystem's, and the flat-file branch is exactly the one that a mock gets wrong.

use std::path::{Path, PathBuf};

// The module is included from its source file rather than reached through `girder_core::skill_catalog`
// because `lib.rs` is wired by a separate change: this test has to compile and run before that landing.
// It is the same file either way, so a behavior that passes here is a behavior of the shipped module.
#[path = "../src/skill_catalog.rs"]
mod catalog;

use catalog::{SkillMeta, discover, parse_frontmatter, render_list};

// ---- fixtures ---------------------------------------------------------------------------------

/// An isolated directory, so one test's files cannot be seen by another's.
fn scratch(name: &str) -> PathBuf {
    static COUNTER: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
    let n = COUNTER.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let dir = std::env::temp_dir().join(format!(
        "girder-skill-catalog-{name}-{}-{n}",
        std::process::id()
    ));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("created the scratch directory");
    dir
}

/// Writes a file, creating its parent directories.
fn write_file(path: &Path, contents: &str) {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).expect("created the parent directory");
    }
    std::fs::write(path, contents).expect("wrote the file");
}

/// Writes `<root>/<name>/SKILL.md` with a well-formed front matter.
fn write_bundle(root: &Path, name: &str, description: &str) {
    write_file(
        &root.join(name).join("SKILL.md"),
        &format!("---\nname: {name}\ndescription: {description}\n---\n\n# {name}\n"),
    );
}

fn meta(name: &str, description: &str) -> SkillMeta {
    SkillMeta {
        name: name.to_owned(),
        description: description.to_owned(),
        path: PathBuf::from(format!("/skills/{name}/SKILL.md")),
    }
}

// ---- parse_frontmatter ------------------------------------------------------------------------

#[test]
fn parses_a_normal_frontmatter() {
    let text = "---\nname: my-skill\ndescription: does something\n---\n# body\n";
    assert_eq!(
        parse_frontmatter(text),
        Some(("my-skill".to_owned(), "does something".to_owned()))
    );
}

#[test]
fn parses_fields_in_any_order_and_ignores_unknown_keys() {
    let text = "---\ndescription: does something\nlicense: MIT\nname: my-skill\n---\nbody\n";
    assert_eq!(
        parse_frontmatter(text),
        Some(("my-skill".to_owned(), "does something".to_owned()))
    );
}

#[test]
fn a_missing_name_is_not_a_skill() {
    assert_eq!(
        parse_frontmatter("---\ndescription: does something\n---\nbody\n"),
        None
    );
}

#[test]
fn a_missing_description_is_not_a_skill() {
    assert_eq!(parse_frontmatter("---\nname: my-skill\n---\nbody\n"), None);
}

#[test]
fn a_blank_name_or_description_is_not_a_skill() {
    assert_eq!(
        parse_frontmatter("---\nname:   \ndescription: x\n---\n"),
        None
    );
    assert_eq!(parse_frontmatter("---\nname: x\ndescription:\n---\n"), None);
}

#[test]
fn no_frontmatter_is_not_a_skill() {
    assert_eq!(parse_frontmatter("# just a heading\nname: x\n"), None);
    assert_eq!(parse_frontmatter(""), None);
}

#[test]
fn an_unterminated_frontmatter_is_not_a_skill() {
    // The closing fence is what proves the block ended; without it, the "front matter" is just prose
    // that happens to start with a rule.
    assert_eq!(parse_frontmatter("---\nname: x\ndescription: y\n"), None);
}

#[test]
fn a_leading_byte_order_mark_is_tolerated() {
    let text = "\u{feff}---\nname: x\ndescription: y\n---\n";
    assert_eq!(
        parse_frontmatter(text),
        Some(("x".to_owned(), "y".to_owned()))
    );
}

#[test]
fn crlf_line_endings_are_tolerated() {
    let text = "---\r\nname: x\r\ndescription: y\r\n---\r\nbody\r\n";
    assert_eq!(
        parse_frontmatter(text),
        Some(("x".to_owned(), "y".to_owned()))
    );
}

#[test]
fn a_double_quoted_description_keeps_its_text() {
    let text = "---\nname: x\ndescription: \"does: something\"\n---\n";
    assert_eq!(
        parse_frontmatter(text),
        Some(("x".to_owned(), "does: something".to_owned()))
    );
}

#[test]
fn a_double_quoted_description_decodes_escapes() {
    let text = "---\nname: x\ndescription: \"say \\\"hi\\\"\\tthen stop\"\n---\n";
    assert_eq!(
        parse_frontmatter(text),
        Some(("x".to_owned(), "say \"hi\"\tthen stop".to_owned()))
    );
}

#[test]
fn a_single_quoted_description_keeps_an_apostrophe() {
    let text = "---\nname: x\ndescription: 'the user''s own words'\n---\n";
    assert_eq!(
        parse_frontmatter(text),
        Some(("x".to_owned(), "the user's own words".to_owned()))
    );
}

#[test]
fn an_unterminated_quote_drops_the_key() {
    let text = "---\nname: x\ndescription: \"never closed\n---\n";
    assert_eq!(parse_frontmatter(text), None);
}

#[test]
fn a_folded_multiline_description_becomes_one_line() {
    let text = "---\nname: x\ndescription: does something\n  that spans lines\n  of a file\n---\n";
    assert_eq!(
        parse_frontmatter(text),
        Some((
            "x".to_owned(),
            "does something that spans lines of a file".to_owned()
        ))
    );
}

#[test]
fn a_block_scalar_description_becomes_one_line() {
    let text = "---\nname: x\ndescription: >\n  does something\n  on two lines\n---\n";
    assert_eq!(
        parse_frontmatter(text),
        Some(("x".to_owned(), "does something on two lines".to_owned()))
    );
    let literal = "---\nname: x\ndescription: |-\n  line one\n  line two\n---\n";
    assert_eq!(
        parse_frontmatter(literal),
        Some(("x".to_owned(), "line one line two".to_owned()))
    );
}

#[test]
fn a_trailing_comment_is_dropped_but_a_url_fragment_is_not() {
    let text = "---\nname: x\ndescription: does something # why\n---\n";
    assert_eq!(
        parse_frontmatter(text),
        Some(("x".to_owned(), "does something".to_owned()))
    );
    let url = "---\nname: x\ndescription: see https://example.com/page#section\n---\n";
    assert_eq!(
        parse_frontmatter(url),
        Some((
            "x".to_owned(),
            "see https://example.com/page#section".to_owned()
        ))
    );
}

#[test]
fn an_indented_key_is_not_a_field() {
    // Only unindented keys are read, so the only `name:` here belongs to `metadata` and the skill has
    // no name. Reading the nested one would silently invent a skill from a metadata map.
    let text = "---\nmetadata:\n  name: nested\ndescription: real\n---\n";
    assert_eq!(parse_frontmatter(text), None);
}

#[test]
fn the_first_occurrence_of_a_key_wins() {
    let text = "---\nname: first\ndescription: d\nname: second\n---\n";
    assert_eq!(
        parse_frontmatter(text),
        Some(("first".to_owned(), "d".to_owned()))
    );
}

// ---- discover ---------------------------------------------------------------------------------

#[test]
fn discovers_directory_bundles_flat_files_and_skips_everything_else() {
    let root = scratch("discover-shapes");
    write_bundle(&root, "alpha", "first skill");
    write_file(
        &root.join("beta.md"),
        "---\nname: beta\ndescription: flat skill\n---\nbody\n",
    );
    // A directory with no SKILL.md is not a bundle.
    write_file(&root.join("not-a-skill").join("README.md"), "hi");
    // A non-Markdown file at the root is not a flat skill.
    write_file(
        &root.join("notes.txt"),
        "---\nname: notes\ndescription: n\n---\n",
    );
    // A dot-directory is bookkeeping, not a skill.
    write_bundle(&root, ".system", "internal");
    // A malformed bundle is skipped, and must not hide the valid ones.
    write_file(&root.join("broken").join("SKILL.md"), "# no frontmatter\n");

    let found = discover(std::slice::from_ref(&root));

    let names: Vec<&str> = found.iter().map(|skill| skill.name.as_str()).collect();
    assert_eq!(names, vec!["alpha", "beta"]);
    assert_eq!(found[0].path, root.join("alpha").join("SKILL.md"));
    assert_eq!(found[1].path, root.join("beta.md"));
    assert_eq!(found[0].description, "first skill");
    assert_eq!(found[1].description, "flat skill");
}

#[test]
fn a_missing_root_is_not_an_error() {
    let root = scratch("missing-root");
    let absent = root.join("does-not-exist");
    let skill_dir = scratch("missing-root-present");
    write_bundle(&skill_dir, "alpha", "first skill");

    assert!(discover(std::slice::from_ref(&absent)).is_empty());
    // A missing root before a present one must not short-circuit the scan.
    let found = discover(&[absent, skill_dir]);
    assert_eq!(found.len(), 1);
}

#[test]
fn an_earlier_root_wins_a_duplicate_name() {
    let project = scratch("duplicate-project");
    let user = scratch("duplicate-user");
    write_bundle(&project, "review", "the project's review");
    write_bundle(&user, "review", "the user's review");
    write_bundle(&user, "other", "another skill");

    let found = discover(&[project, user]);

    assert_eq!(found.len(), 2);
    assert_eq!(found[0].name, "review");
    assert_eq!(found[0].description, "the project's review");
    assert_eq!(found[1].name, "other");
}

#[test]
fn a_later_entry_within_one_root_does_not_override_the_first() {
    // Both entries would claim `same`. Entries are visited in file-name order, and the directory `same`
    // sorts before the file `same.md`, so the bundle wins. The point of the test is that exactly one
    // entry survives: two entries under one name would make a lookup depend on scan order.
    let root = scratch("duplicate-one-root");
    write_file(
        &root.join("same.md"),
        "---\nname: same\ndescription: the flat one\n---\n",
    );
    write_bundle(&root, "same", "the bundle one");

    let found = discover(std::slice::from_ref(&root));

    assert_eq!(found.len(), 1);
    assert_eq!(found[0].description, "the bundle one");
}

#[test]
fn discovery_is_deterministic() {
    let root = scratch("deterministic");
    write_bundle(&root, "zeta", "z");
    write_bundle(&root, "alpha", "a");
    write_bundle(&root, "middle", "m");

    let first = discover(std::slice::from_ref(&root));
    let second = discover(std::slice::from_ref(&root));

    assert_eq!(first, second);
    let names: Vec<&str> = first.iter().map(|skill| skill.name.as_str()).collect();
    assert_eq!(names, vec!["alpha", "middle", "zeta"]);
}

// ---- render_list ------------------------------------------------------------------------------

#[test]
fn renders_one_line_per_skill_sorted_by_name() {
    let skills = vec![meta("zeta", "last"), meta("alpha", "first")];
    assert_eq!(
        render_list(&skills),
        "2 skills available\n- `alpha`: first\n- `zeta`: last"
    );
}

#[test]
fn renders_a_single_skill_without_a_plural() {
    assert_eq!(
        render_list(&[meta("only", "one")]),
        "1 skill available\n- `only`: one"
    );
}

#[test]
fn renders_an_empty_catalog_as_a_sentence() {
    assert_eq!(render_list(&[]), "no skills are available");
}

#[test]
fn a_multiline_description_cannot_break_the_line_format() {
    let mut skill = meta("x", "first line");
    skill.description = "first line\nsecond line".to_owned();
    assert_eq!(
        render_list(&[skill]),
        "1 skill available\n- `x`: first line second line"
    );
}

#[test]
fn markup_in_a_description_is_escaped() {
    let skill = meta("x", "closes </available_skills> & opens <b>");
    assert_eq!(
        render_list(&[skill]),
        "1 skill available\n- `x`: closes &lt;/available_skills&gt; &amp; opens &lt;b&gt;"
    );
}
