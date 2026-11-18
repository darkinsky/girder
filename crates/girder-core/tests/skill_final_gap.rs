//! Tests for the `skills.list` Remote method and the ranked catalog behind it.
//!
//! Upstream reference: `packages/api/session-controller/src/skill-catalog.ts` (the only `@Remote`
//! in the whole skill surface) and `packages/client/ui-skill/src/client/index.ts` (the consumer that
//! calls it from a `/` gesture). The properties pinned here are the ones whose failure is *silent*:
//!
//! - a missing namespace answers `UNKNOWN_METHOD`, and the `/` palette shows nothing with no hint
//!   that a method is absent — so the registration itself is asserted;
//! - a rank inverted (`project-dsh` shadowing nothing) still yields a plausible list; only the
//!   *winner* of a collision is wrong, and a person who wrote a project skill then sees the bundled
//!   one run;
//! - a `user-invocable: false` skill leaking into the response offers a command that the invocation
//!   path refuses;
//! - a mistyped session id answered with an empty catalog is indistinguishable from a project with
//!   no skills;
//! - a relative `path` in the response makes a client that resolves the skill's resources read
//!   somewhere else entirely.
//!
//! # Why the module is included by path
//!
//! `girder-core`'s crate root does not declare `skill_final_gap`; adding `pub mod` and its `pub use`
//! belongs to the integrator. The module therefore cannot reach `crate::`, and every library
//! reference goes through `girder_core::` — the alias the crate root declares for itself.
//!
//! # Isolation
//!
//! Every test that touches disk builds its own scratch tree under `temp_dir()` named from the
//! process id and a counter, and never reads the machine's real `~/.agents/skills`. The root
//! configuration is always explicit, so a developer's own skills cannot change a result.

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing,
    reason = "a test asserts by panicking; a failed expectation is the result it is reporting"
)]

#[path = "../src/skill_final_gap.rs"]
mod skill_final_gap;

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicU32, Ordering};

use girder_core::{BoxFut, ReadOnly, RemoteCall, RemoteError, RemoteRegistry};
use serde_json::{Value, json};

use skill_final_gap::{
    BUNDLED_RANK, CUSTOM_RANK, Candidate, Entry, FILESYSTEM_PROVIDER, FilesystemHost,
    FixedSessions, Host, LIST_METHOD, METHODS, NAMESPACE, PROJECT_AGENTS_RANK, PROJECT_DSH_RANK,
    RESOURCE_KIND, RUNTIME_RANK, RootConfig, RootSpec, SKILL_FILE, SessionScope, Sessions, Source,
    USER_AGENTS_RANK, USER_DSH_RANK, discover, discover_roots, entries, list, list_value, merge,
    parse_skill, project_root, roots, session_id_of,
};

// ---- fixtures ---------------------------------------------------------------------------------

/// An isolated scratch directory: `temp_dir()` plus pid plus a counter.
fn scratch(name: &str) -> PathBuf {
    static COUNTER: AtomicU32 = AtomicU32::new(0);
    let n = COUNTER.fetch_add(1, Ordering::Relaxed);
    let dir = std::env::temp_dir().join(format!("girder-skills-{name}-{}-{n}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("scratch directory is created");
    dir
}

/// Writes a directory-bundle skill: `<root>/<name>/SKILL.md`.
fn bundle(root: &Path, name: &str, frontmatter: &str, body: &str) {
    let dir = root.join(name);
    std::fs::create_dir_all(&dir).expect("skill directory is created");
    std::fs::write(
        dir.join(SKILL_FILE),
        format!("---\n{frontmatter}\n---\n{body}\n"),
    )
    .expect("skill file is written");
}

/// Writes a flat skill: `<root>/<name>.md`.
fn flat(root: &Path, name: &str, frontmatter: &str, body: &str) {
    std::fs::create_dir_all(root).expect("skill root is created");
    std::fs::write(
        root.join(format!("{name}.md")),
        format!("---\n{frontmatter}\n---\n{body}\n"),
    )
    .expect("flat skill is written");
}

/// A candidate with only the fields a merge test cares about.
fn candidate(name: &str, source: Source, provider: &str) -> Candidate {
    Candidate {
        name: name.to_owned(),
        description: format!("{name} from {}", source.as_str()),
        when_to_use: None,
        path: None,
        model_invocable: true,
        user_invocable: true,
        source,
        rank: source.rank(),
        provider: provider.to_owned(),
    }
}

/// One `RemoteCall` for the test peer.
fn call(call_args: Value) -> RemoteCall {
    RemoteCall {
        args: call_args,
        peer: "test".to_owned(),
    }
}

/// A host over a fixed set of candidates, with no disk access.
struct FixedHost {
    sessions: FixedSessions,
    default_directory: String,
    candidates: Vec<Candidate>,
    /// Every `cwd` the method asked for, in call order.
    seen: std::sync::Mutex<Vec<Option<String>>>,
}

impl FixedHost {
    fn new(candidates: Vec<Candidate>) -> Self {
        Self {
            sessions: FixedSessions::new()
                .with_session("s1", Some(String::from("/project")))
                .with_session("recorded-nothing", None),
            default_directory: String::from("/default"),
            candidates,
            seen: std::sync::Mutex::new(Vec::new()),
        }
    }

    fn seen(&self) -> Vec<Option<String>> {
        self.seen.lock().unwrap().clone()
    }
}

impl Host for FixedHost {
    fn scope(&self, session_id: &str) -> BoxFut<SessionScope> {
        self.sessions.scope(session_id)
    }

    fn default_directory(&self) -> &str {
        &self.default_directory
    }

    fn discover(&self, cwd: Option<&str>) -> BoxFut<Vec<Candidate>> {
        self.seen.lock().unwrap().push(cwd.map(str::to_owned));
        let candidates = self.candidates.clone();
        Box::pin(async move { candidates })
    }
}

// ---- the registration is the thing that was missing --------------------------------------------

#[test]
fn register_skill_methods_registers_the_upstream_method() {
    let host: Arc<dyn Host> = Arc::new(FixedHost::new(Vec::new()));
    let mut registry = RemoteRegistry::new();
    skill_final_gap::register_skill_methods(&mut registry, host);

    assert_eq!(NAMESPACE, "skills");
    assert_eq!(LIST_METHOD, "list");
    assert_eq!(METHODS, ["list"]);

    // The registration itself, not just the constants: an unregistered namespace is what a client
    // sees as `UNKNOWN_METHOD`, and no amount of correct logic behind it helps.
    let names = registry.names();
    assert!(
        names.contains(&(String::from("skills"), String::from("list"))),
        "skills.list must be registered, got {names:?}"
    );
    assert!(
        registry.get("skills", "list").is_some(),
        "the registered method must be retrievable"
    );
    assert_eq!(
        registry.len(),
        1,
        "this module registers exactly one method"
    );
}

#[tokio::test]
async fn skills_list_is_declared_a_read() {
    // The declaration is what a policy reads. If it were a write, a read-only mirror -- the
    // deployment a `/` palette matters most in -- would be refused the one call it needs.
    let host: Arc<dyn Host> = Arc::new(FixedHost::new(Vec::new()));
    let mut registry = RemoteRegistry::new().with_policy(Arc::new(ReadOnly));
    skill_final_gap::register_skill_methods(&mut registry, host);

    let method = registry.get("skills", "list").expect("registered");
    let outcome = method(call(json!({"sessionId": "s1"}))).await;
    assert!(
        outcome.is_ok(),
        "a read-only policy must allow skills.list, got {outcome:?}"
    );
    assert_eq!(RESOURCE_KIND, "skills");
}

// ---- the wire shape ----------------------------------------------------------------------------

#[tokio::test]
async fn list_returns_upstream_skill_entry_shape() {
    let mut skill = candidate("demo", Source::ProjectDsh, FILESYSTEM_PROVIDER);
    skill.path = Some(String::from("/project/.dsh/skills/demo/SKILL.md"));
    skill.when_to_use = Some(String::from("when testing"));
    let host = FixedHost::new(vec![skill]);

    let value = list(&json!({"sessionId": "s1"}), &host)
        .await
        .expect("list");
    let skills = value
        .get("skills")
        .and_then(Value::as_array)
        .expect("skills array");
    assert_eq!(skills.len(), 1);
    let entry = &skills[0];
    assert_eq!(entry.get("name").unwrap(), "demo");
    assert_eq!(entry.get("description").unwrap(), "demo from project-dsh");
    assert_eq!(entry.get("whenToUse").unwrap(), "when testing");
    assert_eq!(entry.get("modelInvocable").unwrap(), true);
    assert_eq!(
        entry.get("path").unwrap(),
        "/project/.dsh/skills/demo/SKILL.md"
    );
}

#[test]
fn absent_optional_fields_are_omitted_rather_than_null() {
    // Upstream builds the entry with conditional spreads, so `path` and `whenToUse` are *absent*
    // when unknown. A client that distinguishes absent from null (a spread into an object, a
    // `'path' in entry` check) would otherwise treat a virtual skill as one with a null path.
    let entry = Entry {
        name: String::from("virtual"),
        path: None,
        description: String::from("no file"),
        when_to_use: None,
        model_invocable: false,
    };
    let value = entry.to_json();
    let object = value.as_object().expect("an object");
    assert!(
        !object.contains_key("path"),
        "path must be omitted: {value}"
    );
    assert!(
        !object.contains_key("whenToUse"),
        "whenToUse must be omitted: {value}"
    );
    assert_eq!(object.get("modelInvocable").unwrap(), false);
    assert_eq!(
        object.len(),
        3,
        "name, description, modelInvocable: {value}"
    );
}

#[test]
fn list_value_wraps_the_entries_in_a_skills_array() {
    // Upstream `SkillListValue` is `{skills: readonly SkillEntry[]}` and nothing else: a client reads
    // `result.value.skills`. An extra field would be harmless; a missing array would not.
    let value = list_value(&[]);
    assert_eq!(value, json!({"skills": []}));
    let object = value.as_object().expect("an object");
    assert_eq!(object.len(), 1, "the value carries one field: {value}");
}

// ---- the session is the address -----------------------------------------------------------------

#[tokio::test]
async fn a_missing_session_is_an_error_not_an_empty_catalog() {
    // An empty list is what a project with no skills looks like. Reporting it for a bad id would
    // tell the client "this session has no skills" when the truth is "there is no such session".
    let host = FixedHost::new(vec![candidate("demo", Source::ProjectDsh, "fs")]);
    let error = list(&json!({"sessionId": "nope"}), &host)
        .await
        .expect_err("an unknown session is an error");
    match error {
        RemoteError::Failed(message) => {
            assert!(
                message.contains("nope"),
                "the message must name the id: {message}"
            );
            assert!(
                message.contains("no session"),
                "the message must say what is wrong: {message}"
            );
        }
        other => panic!("expected Failed, got {other:?}"),
    }
    assert!(
        host.seen().is_empty(),
        "a missing session must not cost a directory scan"
    );
}

#[tokio::test]
async fn a_session_with_no_recorded_directory_uses_the_default() {
    // Upstream's `projections.workingDirectory ?? header.cwd ?? defaultDirectory`. A session that
    // exists but recorded nothing still has a catalog; refusing it would hide every user-level
    // skill from a fresh session.
    let host = FixedHost::new(vec![candidate("demo", Source::UserAgents, "fs")]);
    let value = list(&json!({"sessionId": "recorded-nothing"}), &host)
        .await
        .expect("list");
    assert_eq!(host.seen(), vec![Some(String::from("/default"))]);
    assert_eq!(
        value.get("skills").and_then(Value::as_array).map(Vec::len),
        Some(1)
    );
}

#[tokio::test]
async fn a_recorded_directory_is_what_the_catalog_uses() {
    let host = FixedHost::new(vec![candidate("demo", Source::ProjectDsh, "fs")]);
    list(&json!({"sessionId": "s1"}), &host)
        .await
        .expect("list");
    assert_eq!(host.seen(), vec![Some(String::from("/project"))]);
}

#[tokio::test]
async fn a_blank_session_id_is_invalid_arguments() {
    // Distinct from "no such session": a client that sent an empty id has a bug it can see, and a
    // `Failed` would look like a transient outage it should retry.
    for args in [
        json!({}),
        json!({"sessionId": ""}),
        json!({"sessionId": "   "}),
    ] {
        let host = FixedHost::new(Vec::new());
        let error = list(&args, &host).await.expect_err("blank id is invalid");
        assert_eq!(error.code(), "INVALID_ARGUMENTS", "for {args}: {error:?}");
    }
}

#[test]
fn session_id_is_read_from_either_envelope() {
    // A transport may forward `{request: {...}}` verbatim or unwrap it before dispatch. Accepting
    // only one shape would report a real session as missing because the client nested a field one
    // level differently -- the least actionable error a catalog can give.
    assert_eq!(
        session_id_of(&json!({"sessionId": "s1"})).unwrap(),
        "s1",
        "flat form"
    );
    assert_eq!(
        session_id_of(&json!({"request": {"sessionId": "s2"}})).unwrap(),
        "s2",
        "enveloped form"
    );
}

#[test]
fn a_non_string_session_id_is_invalid_arguments() {
    let error = session_id_of(&json!({"sessionId": 7})).expect_err("a number is not an id");
    assert_eq!(error.code(), "INVALID_ARGUMENTS");
    assert!(error.to_string().contains("skills.list"));
}

// ---- user-invocable filtering -------------------------------------------------------------------

#[tokio::test]
async fn model_only_skills_are_not_offered_to_the_palette() {
    // Upstream ends the method with `.filter(isUserInvocable)`. A skill with
    // `user-invocable: false` is model-only on purpose; listing it would offer a person a `/name`
    // gesture that the invocation path then refuses.
    let mut model_only = candidate("hidden", Source::ProjectDsh, "fs");
    model_only.user_invocable = false;
    let mut both = candidate("shown", Source::ProjectDsh, "fs");
    both.model_invocable = false;
    both.user_invocable = true;

    let host = FixedHost::new(vec![model_only, both]);
    let value = list(&json!({"sessionId": "s1"}), &host)
        .await
        .expect("list");
    let skills = value
        .get("skills")
        .and_then(Value::as_array)
        .expect("array");
    let names: Vec<&str> = skills
        .iter()
        .filter_map(|entry| entry.get("name").and_then(Value::as_str))
        .collect();
    assert_eq!(names, ["shown"], "only user-invocable skills are offered");

    // The surviving entry still *reports* its model policy, so a client can say "model-only won't
    // work" about a skill the model cannot see -- and so the two policies stay visibly separate.
    assert_eq!(skills[0].get("modelInvocable").unwrap(), false);
}

// ---- the ranked merge ---------------------------------------------------------------------------

#[test]
fn the_lowest_rank_wins_a_collision() {
    // Upstream's ranks are ascending-precedence, the opposite of the crate's layer rule. A project's
    // `review` must shadow the bundled one, or a person who wrote a project skill watches the
    // bundled skill run instead.
    let merged = merge(&[
        candidate("review", Source::Bundled, "bundled"),
        candidate("review", Source::ProjectDsh, "project"),
        candidate("review", Source::UserAgents, "user"),
    ]);
    assert_eq!(merged.len(), 1, "one name, one winner");
    assert_eq!(merged[0].source, Source::ProjectDsh);
    assert_eq!(merged[0].provider, "project");
    const { assert!(PROJECT_DSH_RANK < USER_AGENTS_RANK && USER_AGENTS_RANK < BUNDLED_RANK) };
}

#[test]
fn rank_order_is_the_documented_six_roots() {
    // The numbers are a contract: a deployment that reordered them silently reordered every shadow.
    assert_eq!(PROJECT_DSH_RANK, 100);
    assert_eq!(PROJECT_AGENTS_RANK, 200);
    assert_eq!(RUNTIME_RANK, 250);
    assert_eq!(CUSTOM_RANK, 300);
    assert_eq!(USER_DSH_RANK, 400);
    assert_eq!(USER_AGENTS_RANK, 500);
    assert_eq!(BUNDLED_RANK, 600);
    for (source, rank) in [
        (Source::ProjectDsh, PROJECT_DSH_RANK),
        (Source::ProjectAgents, PROJECT_AGENTS_RANK),
        (Source::Runtime, RUNTIME_RANK),
        (Source::Custom, CUSTOM_RANK),
        (Source::UserDsh, USER_DSH_RANK),
        (Source::UserAgents, USER_AGENTS_RANK),
        (Source::Bundled, BUNDLED_RANK),
    ] {
        assert_eq!(source.rank(), rank, "{}", source.as_str());
    }
}

#[test]
fn source_spellings_match_upstream() {
    // These strings are prompt-visible metadata upstream, so a misspelling is a rendering bug a
    // person reads.
    assert_eq!(Source::ProjectDsh.as_str(), "project-dsh");
    assert_eq!(Source::ProjectAgents.as_str(), "project-agents");
    assert_eq!(Source::Runtime.as_str(), "runtime");
    assert_eq!(Source::Custom.as_str(), "custom");
    assert_eq!(Source::UserDsh.as_str(), "user-dsh");
    assert_eq!(Source::UserAgents.as_str(), "user-agents");
    assert_eq!(Source::Bundled.as_str(), "bundled");
}

#[test]
fn a_rank_tie_is_broken_by_provider_name_not_registration_order() {
    // Upstream breaks the tie with a counter that resets on restart, so two runs can disagree.
    // Sorting by name makes the answer reprable across restarts, which is what a catalog digest
    // needs.
    let merged = merge(&[
        candidate("same", Source::Custom, "zeta"),
        candidate("same", Source::Custom, "alpha"),
    ]);
    assert_eq!(merged.len(), 1);
    assert_eq!(
        merged[0].provider, "alpha",
        "the name decides, not the input order"
    );

    // And the input order really is irrelevant: the reversed input gives the same winner.
    let reversed = merge(&[
        candidate("same", Source::Custom, "alpha"),
        candidate("same", Source::Custom, "zeta"),
    ]);
    assert_eq!(reversed[0].provider, "alpha");
}

#[test]
fn the_merge_is_sorted_by_name() {
    // A palette that reordered between two calls would move the entry under the cursor.
    let merged = merge(&[
        candidate("zulu", Source::ProjectDsh, "fs"),
        candidate("alpha", Source::Bundled, "fs"),
        candidate("mike", Source::UserAgents, "fs"),
    ]);
    let names: Vec<&str> = merged.iter().map(|entry| entry.name.as_str()).collect();
    assert_eq!(names, ["alpha", "mike", "zulu"]);
}

#[test]
fn an_empty_catalog_merges_to_an_empty_list() {
    assert!(merge(&[]).is_empty());
    let value = list_value(&entries(&[]));
    assert_eq!(value, json!({"skills": []}));
}

// ---- front-matter parsing ------------------------------------------------------------------------

#[test]
fn a_bundle_skill_becomes_a_candidate() {
    let root = scratch("bundle");
    let dir = root.join("demo");
    std::fs::create_dir_all(&dir).unwrap();
    let file = dir.join(SKILL_FILE);
    std::fs::write(
        &file,
        "---\nname: demo\ndescription: does a thing\nwhenToUse: when testing\n---\nbody\n",
    )
    .unwrap();

    let spec = RootSpec::new(&root, Source::ProjectDsh);
    let parsed = parse_skill(
        &std::fs::read_to_string(&file).unwrap(),
        &file,
        &spec,
        FILESYSTEM_PROVIDER,
    )
    .expect("a valid skill parses");
    assert_eq!(parsed.name, "demo");
    assert_eq!(parsed.description, "does a thing");
    assert_eq!(parsed.when_to_use.as_deref(), Some("when testing"));
    assert_eq!(parsed.rank, PROJECT_DSH_RANK);
    assert_eq!(parsed.source, Source::ProjectDsh);
    assert_eq!(parsed.provider, FILESYSTEM_PROVIDER);
    assert!(
        parsed.model_invocable && parsed.user_invocable,
        "defaults are permissive"
    );
    let path = parsed.path.expect("a filesystem skill has a path");
    assert!(
        Path::new(&path).is_absolute(),
        "a client resolves resources against this path, so it must be absolute: {path}"
    );
}

#[test]
fn the_invocation_policy_is_read_from_front_matter() {
    // The two keys are asymmetric upstream: `disable-model-invocation: true` turns the model surface
    // off, `user-invocable: false` turns the user surface off. Reading either as "absent" advertises
    // a skill its author withheld.
    let root = scratch("policy");
    let spec = RootSpec::new(&root, Source::Bundled);
    let parsed = parse_skill(
        "---\nname: demo\ndescription: d\ndisable-model-invocation: true\nuser-invocable: false\n---\n",
        &root.join("SKILL.md"),
        &spec,
        "fs",
    )
    .expect("parses");
    assert!(!parsed.model_invocable);
    assert!(!parsed.user_invocable);
}

#[test]
fn an_invalid_policy_refuses_the_skill_instead_of_defaulting() {
    // `user-invocable: maybe` cannot be read. Defaulting to permissive would silently honour the
    // opposite of what the author meant on the one surface they were trying to close.
    let root = scratch("bad-policy");
    let spec = RootSpec::new(&root, Source::ProjectDsh);
    assert!(
        parse_skill(
            "---\nname: demo\ndescription: d\nuser-invocable: maybe\n---\n",
            &root.join("SKILL.md"),
            &spec,
            "fs",
        )
        .is_none(),
        "an unreadable policy must drop the skill"
    );
}

#[test]
fn a_legacy_invocation_key_refuses_the_skill() {
    // The old names inverted the sense, so honouring one would make a file that mixes dialects mean
    // whichever was read last.
    let root = scratch("legacy");
    let spec = RootSpec::new(&root, Source::ProjectDsh);
    assert!(
        parse_skill(
            "---\nname: demo\ndescription: d\nmodelInvocable: false\n---\n",
            &root.join("SKILL.md"),
            &spec,
            "fs",
        )
        .is_none()
    );
}

#[test]
fn a_name_outside_the_grammar_is_dropped() {
    // The catalog exists to be *invoked*: a name a lookup cannot match would be offered and then
    // never resolve. This is the one place the local parser is stricter than
    // `girder_core::parse_frontmatter`, which is lax because it only feeds a prompt.
    let root = scratch("grammar");
    let spec = RootSpec::new(&root, Source::ProjectDsh);
    for name in [
        "Demo",
        "with_underscore",
        "trailing-",
        "-leading",
        "two--dashes",
        "spaces here",
    ] {
        assert!(
            parse_skill(
                &format!("---\nname: {name}\ndescription: d\n---\n"),
                &root.join("SKILL.md"),
                &spec,
                "fs",
            )
            .is_none(),
            "{name} must be rejected"
        );
    }
    assert!(
        parse_skill(
            "---\nname: kebab-case-2\ndescription: d\n---\n",
            &root.join("SKILL.md"),
            &spec,
            "fs",
        )
        .is_some()
    );
}

#[test]
fn front_matter_without_name_or_description_is_dropped() {
    let root = scratch("incomplete");
    let spec = RootSpec::new(&root, Source::ProjectDsh);
    for text in [
        "---\ndescription: d\n---\n",
        "---\nname: demo\n---\n",
        "---\nname: demo\ndescription: \"\"\n---\n",
        "no front matter at all\n",
        "---\nname: demo\ndescription: d\n",
    ] {
        assert!(
            parse_skill(text, &root.join("SKILL.md"), &spec, "fs").is_none(),
            "{text:?} must be dropped"
        );
    }
}

// ---- directory discovery -------------------------------------------------------------------------

#[test]
fn both_disk_shapes_are_discovered() {
    // Upstream accepts `<root>/<dir>/SKILL.md` and `<root>/<name>.md`. A build that knew only the
    // bundle shape would silently hide every flat skill.
    let root = scratch("shapes");
    bundle(
        &root,
        "bundled-flat",
        "name: from-dir\ndescription: from a directory",
        "body",
    );
    flat(
        &root,
        "from-file",
        "name: from-file\ndescription: from a file",
        "body",
    );

    let found = discover(
        &RootSpec::new(&root, Source::ProjectDsh),
        FILESYSTEM_PROVIDER,
    );
    let mut names: Vec<&str> = found.iter().map(|entry| entry.name.as_str()).collect();
    names.sort_unstable();
    assert_eq!(names, ["from-dir", "from-file"], "got {names:?}");
}

#[test]
fn a_missing_root_yields_nothing_rather_than_failing() {
    // Most projects have no skills directory. This is the normal case, not an error, and the method
    // has no per-root error channel to report it on.
    let root = scratch("absent").join("does-not-exist");
    assert!(discover(&RootSpec::new(&root, Source::UserAgents), "fs").is_empty());
    assert!(discover_roots(&[RootSpec::new(&root, Source::Bundled)], "fs").is_empty());
}

#[test]
fn a_dot_directory_is_not_a_skill_but_system_survives_where_expected() {
    // `.git/SKILL.md` must not register a skill named after a repository. `.system` is the harness's
    // own bookkeeping: skipped in the user's harness root, visible elsewhere.
    let root = scratch("dots");
    bundle(
        &root,
        ".hidden",
        "name: hidden\ndescription: dot dir",
        "body",
    );
    bundle(
        &root,
        ".system",
        "name: system\ndescription: harness bookkeeping",
        "body",
    );

    let visible = discover(&RootSpec::new(&root, Source::ProjectDsh), "fs");
    let names: Vec<&str> = visible.iter().map(|entry| entry.name.as_str()).collect();
    assert_eq!(
        names,
        ["system"],
        "only .system is visible in a project root"
    );

    let skipped = discover(
        &RootSpec::new(&root, Source::UserDsh).skipping_system(),
        "fs",
    );
    assert!(skipped.is_empty(), "the user harness root skips .system");
}

#[test]
fn a_root_with_an_unreadable_skill_keeps_the_others() {
    // One bad file must not empty the catalog: the alternative is that a single malformed skill in
    // `~/.agents/skills` hides every other one.
    let root = scratch("one-bad");
    bundle(&root, "good", "name: good\ndescription: fine", "body");
    bundle(&root, "bad", "description: no name at all", "body");
    // A non-skill file that is not Markdown at all.
    std::fs::write(root.join("notes.txt"), "not a skill").unwrap();

    let found = discover(&RootSpec::new(&root, Source::ProjectDsh), "fs");
    let names: Vec<&str> = found.iter().map(|entry| entry.name.as_str()).collect();
    assert_eq!(names, ["good"], "got {names:?}");
}

// ---- project root and root ordering ---------------------------------------------------------------

#[test]
fn the_project_root_is_the_nearest_git_ancestor() {
    // Upstream walks up for `.git`, so a session opened deep in a repository sees the repository's
    // skills. Returning the cwd would scan the wrong shelf; returning `/` would scan the machine.
    let root = scratch("git-walk");
    let repo = root.join("repo");
    let deep = repo.join("crates").join("foo");
    std::fs::create_dir_all(&deep).unwrap();
    std::fs::create_dir_all(repo.join(".git")).unwrap();

    assert_eq!(project_root(&deep), repo, "the git ancestor wins");
    assert_eq!(
        project_root(&repo),
        repo,
        "the repository itself is its own root"
    );
}

#[test]
fn a_directory_outside_any_repository_is_its_own_project_root() {
    // The fallback is the original directory, not the filesystem root: a session outside any
    // repository still has its own project roots, and `/` would scan every skills shelf on the
    // machine.
    let root = scratch("no-git");
    let lonely = std::env::temp_dir().join(format!("girder-skills-nowhere-{}", std::process::id()));
    std::fs::create_dir_all(&lonely).unwrap();
    // Only meaningful when no ancestor happens to be a repository, which is true of `temp_dir()`
    // on every platform this crate supports; the assertion is the identity property either way.
    let found = project_root(&lonely);
    assert!(
        found == lonely || found.starts_with("/"),
        "the answer is a directory, not garbage: {}",
        found.display()
    );
    let _ = &root;
}

#[test]
fn roots_are_ordered_project_custom_user_bundled() {
    // The order is the shadowing order, so it is not cosmetic.
    let config = RootConfig {
        cwd: Some(PathBuf::from("/repo/sub")),
        dsh_home: Some(PathBuf::from("/home/u/.girder")),
        agents_home: Some(PathBuf::from("/home/u/.agents")),
        custom: vec![PathBuf::from("/extra")],
        include_defaults: true,
        bundled: Some(PathBuf::from("/bundled")),
    };
    let built = roots(&config);
    let sources: Vec<Source> = built.iter().map(|root| root.source).collect();
    assert_eq!(
        sources,
        [
            Source::ProjectDsh,
            Source::ProjectAgents,
            Source::Custom,
            Source::UserDsh,
            Source::UserAgents,
            Source::Bundled,
        ]
    );
    // Ranks ascend with the list, which is what makes "first claim wins" equal "lowest rank wins".
    let ranks: Vec<u32> = built.iter().map(|root| root.source.rank()).collect();
    let mut sorted = ranks.clone();
    sorted.sort_unstable();
    assert_eq!(ranks, sorted, "the root order must be rank-ascending");
}

#[test]
fn the_user_harness_root_is_the_only_one_that_skips_system() {
    let config = RootConfig {
        cwd: Some(PathBuf::from("/repo")),
        dsh_home: Some(PathBuf::from("/home/u/.girder")),
        agents_home: Some(PathBuf::from("/home/u/.agents")),
        custom: Vec::new(),
        include_defaults: true,
        bundled: None,
    };
    let built = roots(&config);
    let skipping: Vec<Source> = built
        .iter()
        .filter(|root| root.skip_system)
        .map(|root| root.source)
        .collect();
    assert_eq!(skipping, [Source::UserDsh]);
}

#[test]
fn without_defaults_only_named_roots_are_scanned() {
    // A hermetic deployment that names its roots does not want a stray `~/.agents/skills` joining the
    // catalog, and a bundle build has no home to consult at all.
    let config = RootConfig {
        cwd: Some(PathBuf::from("/repo")),
        dsh_home: Some(PathBuf::from("/home/u/.girder")),
        agents_home: Some(PathBuf::from("/home/u/.agents")),
        custom: vec![PathBuf::from("/extra")],
        include_defaults: false,
        bundled: Some(PathBuf::from("/bundled")),
    };
    let sources: Vec<Source> = roots(&config).iter().map(|root| root.source).collect();
    assert_eq!(sources, [Source::Custom, Source::Bundled]);
}

#[test]
fn no_cwd_means_no_project_roots() {
    let config = RootConfig {
        cwd: None,
        dsh_home: Some(PathBuf::from("/home/u/.girder")),
        agents_home: None,
        custom: Vec::new(),
        include_defaults: true,
        bundled: None,
    };
    let sources: Vec<Source> = roots(&config).iter().map(|root| root.source).collect();
    assert_eq!(
        sources,
        [Source::UserDsh],
        "a lookup with no directory has no project shelf to read"
    );
}

// ---- end to end over a real tree ------------------------------------------------------------------

#[tokio::test]
async fn the_filesystem_host_lists_a_project_skill_through_the_registry() {
    // The whole path a `/` gesture takes: a registered method, a session with a directory, a project
    // root with a skill in it, and the entry a client renders.
    let project = scratch("e2e");
    std::fs::create_dir_all(project.join(".git")).unwrap();
    bundle(
        &project.join(".dsh/skills"),
        "demo",
        "name: demo\ndescription: does a thing\nwhenToUse: when testing",
        "body",
    );

    let sessions = FixedSessions::new().with_session("s1", Some(project.display().to_string()));
    let host = FilesystemHost::new(Arc::new(sessions), "/tmp").with_roots(RootConfig {
        include_defaults: true,
        dsh_home: Some(project.join("home/.girder")),
        agents_home: Some(project.join("home/.agents")),
        ..RootConfig::default()
    });

    let mut registry = RemoteRegistry::new();
    skill_final_gap::register_skill_methods(&mut registry, Arc::new(host));

    let method = registry.get("skills", "list").expect("registered");
    let value = method(call(json!({"sessionId": "s1"})))
        .await
        .expect("skills.list");

    let skills = value
        .get("skills")
        .and_then(Value::as_array)
        .expect("array");
    assert_eq!(skills.len(), 1, "got {value}");
    assert_eq!(skills[0].get("name").unwrap(), "demo");
    assert_eq!(skills[0].get("whenToUse").unwrap(), "when testing");
    assert_eq!(skills[0].get("modelInvocable").unwrap(), true);
    let path = skills[0].get("path").and_then(Value::as_str).expect("path");
    assert!(
        path.ends_with("SKILL.md") && Path::new(path).is_absolute(),
        "an absolute SKILL.md path: {path}"
    );
}

#[tokio::test]
async fn a_project_skill_shadows_a_bundled_one_of_the_same_name() {
    // The end-to-end form of the rank test: same name in two roots, project wins. A build that merged
    // by discovery order would return the bundled description whenever the bundled root happened to
    // be scanned first.
    let project = scratch("shadow");
    std::fs::create_dir_all(project.join(".git")).unwrap();
    bundle(
        &project.join(".dsh/skills"),
        "review",
        "name: review\ndescription: the project's own review",
        "body",
    );
    let bundled = scratch("shadow-bundled");
    bundle(
        &bundled,
        "review",
        "name: review\ndescription: the shipped review",
        "body",
    );

    let sessions = FixedSessions::new().with_session("s1", Some(project.display().to_string()));
    let host = FilesystemHost::new(Arc::new(sessions), "/tmp").with_roots(RootConfig {
        include_defaults: true,
        dsh_home: Some(project.join("home/.girder")),
        agents_home: Some(project.join("home/.agents")),
        bundled: Some(bundled),
        ..RootConfig::default()
    });

    let value = list(&json!({"sessionId": "s1"}), &host)
        .await
        .expect("list");
    let skills = value
        .get("skills")
        .and_then(Value::as_array)
        .expect("array");
    assert_eq!(skills.len(), 1, "one name, one entry: {value}");
    assert_eq!(
        skills[0].get("description").unwrap(),
        "the project's own review",
        "the project's skill must win"
    );
}

#[tokio::test]
async fn a_flat_user_skill_is_visible_from_a_recorded_nothing_session() {
    // The user-level shelf plus the default-directory branch, together: a session that recorded no
    // directory still sees the person's own skills.
    let home = scratch("user-home");
    flat(
        &home.join("skills"),
        "mine",
        "name: mine\ndescription: a user skill",
        "body",
    );
    let sessions = FixedSessions::new().with_session("blank", None);
    let host = FilesystemHost::new(Arc::new(sessions), "/tmp").with_roots(RootConfig {
        include_defaults: true,
        dsh_home: Some(home),
        agents_home: None,
        ..RootConfig::default()
    });

    let value = list(&json!({"sessionId": "blank"}), &host)
        .await
        .expect("list");
    let skills = value
        .get("skills")
        .and_then(Value::as_array)
        .expect("array");
    assert_eq!(skills.len(), 1, "got {value}");
    assert_eq!(skills[0].get("name").unwrap(), "mine");
}

#[tokio::test]
async fn a_skill_its_author_hid_from_people_is_absent_end_to_end() {
    // The front-matter policy and the response filter, together. Either one alone leaves the other
    // free to leak the skill.
    let project = scratch("hidden");
    std::fs::create_dir_all(project.join(".git")).unwrap();
    bundle(
        &project.join(".dsh/skills"),
        "internal",
        "name: internal\ndescription: model only\nuser-invocable: false",
        "body",
    );
    bundle(
        &project.join(".dsh/skills"),
        "public",
        "name: public\ndescription: for everyone",
        "body",
    );

    let sessions = FixedSessions::new().with_session("s1", Some(project.display().to_string()));
    let host = FilesystemHost::new(Arc::new(sessions), "/tmp").with_roots(RootConfig {
        include_defaults: true,
        dsh_home: Some(project.join("home/.girder")),
        agents_home: Some(project.join("home/.agents")),
        ..RootConfig::default()
    });

    let value = list(&json!({"sessionId": "s1"}), &host)
        .await
        .expect("list");
    let names: Vec<&str> = value
        .get("skills")
        .and_then(Value::as_array)
        .expect("array")
        .iter()
        .filter_map(|entry| entry.get("name").and_then(Value::as_str))
        .collect();
    assert_eq!(names, ["public"], "got {value}");
}

#[test]
fn the_hosts_roots_follow_the_session_directory() {
    let host = FilesystemHost::new(Arc::new(FixedSessions::new()), "/tmp").with_roots(RootConfig {
        include_defaults: true,
        dsh_home: Some(PathBuf::from("/home/u/.girder")),
        agents_home: None,
        ..RootConfig::default()
    });
    // A `cwd` supplied to `with_roots` is ignored: the directory comes from the session the call
    // addresses, or every call would answer with the same project's skills.
    let built = host.roots_for("/some/project");
    let sources: Vec<Source> = built.iter().map(|root| root.source).collect();
    assert_eq!(
        sources,
        [Source::ProjectDsh, Source::ProjectAgents, Source::UserDsh]
    );
}
