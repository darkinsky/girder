//! The `skills.list` Remote method: the session-addressed skill catalog a client cannot do without.
//!
//! # What upstream actually has
//!
//! Upstream's skill support is six packages plus two experimental ones, and exactly **one** of them
//! contributes a Remote method. The inventory, with the surface each one adds:
//!
//! | upstream package | surface |
//! |---|---|
//! | `packages/skill/skill` (595 LOC) | the registry: `SkillRegistry` service, provider protocol (`list`/`get`), runtime registration, scoped layers, ranked merge, `isSkillName` / `isModelInvocable` / `isUserInvocable` / `renderSkillContent` |
//! | `packages/skill/skill-filesystem` (894 LOC) | the filesystem provider: six ranked roots, front-matter parsing, invocation-policy parsing, watchers, `FileSystemSkillProvider` |
//! | `packages/skill/tool-skill` (339 LOC) | the model-facing `skill` tool plus catalog rendering (`catalog_description`, digest, incomplete notice) |
//! | `packages/api/session-controller/src/skill-catalog.ts` | **`skills/list`** — `SessionSkillCatalog`, one `@Remote` method, namespace `skills` |
//! | `packages/client/ui-skill` (291 LOC) | the client `/`-command surface that *consumes* `skills/list` |
//! | `packages/skill/skill-office` (104 LOC) | a provider over packaged Office skills |
//! | `packages/skill/skill-badge`, `packages/experimental/skill-badge` (47 LOC) | a one-skill bundled provider |
//! | `packages/skill/tool-workspace-dependencies` | workspace runtime resolution, no skill surface |
//!
//! A scan of `@Remote` across all of them finds the decorator in `skill-catalog.ts` and nowhere else.
//! So the wire contract is one method: `skills/list({sessionId}) -> {skills: SkillEntry[]}`.
//!
//! # What this build already had, and the one hole it left
//!
//! Four local modules cover the domain, and a symbol-by-symbol grep of upstream's exports shows the
//! split clearly:
//!
//! - [`girder_core::skill`] — the registry: `Summary`/`Definition`/`Provider`/`Registry`/`Static`,
//!   ranked by layer with first-wins collision resolution. Covers `SkillRegistry`'s **merge** and
//!   **load**, not its provider protocol.
//! - [`girder_core::skill::catalog`] — directory discovery: bundle and flat-Markdown shapes,
//!   front-matter reading, prompt rendering. Covers a *subset* of `skill-filesystem`'s discovery:
//!   it has no sources, no ranks, no `whenToUse`, and no invocation policy, because it only ever
//!   needed to feed a prompt.
//! - [`girder_core::skill::policy`] — the policy and catalog *protocol*: `InvocationPolicy`,
//!   `parse_frontmatter_block`, `parse_invocation_policy`, `is_skill_name`, `render_skill_content`,
//!   catalog entries/digest/gestures, `check_model_load`. Covers most of `packages/skill/skill`'s
//!   pure functions and `skill-filesystem`'s front-matter layer.
//! - `skill_tool` — the model-facing `skill` tool. Covers `tool-skill`.
//!
//! What none of them had is a **`skills` namespace on the Remote registry**. A grep for
//! `"skills"` as a registered namespace across `crates/` returns nothing, and the transport's own
//! method list never mentioned it — so this was not "implemented but unregistered" (the hypothesis
//! worth checking first): the method, its request type, and its response type were all absent. The
//! `ui-skill` client calls `skills.list` on a `/` gesture, so a build without it has a client
//! command that fails with `UNKNOWN_METHOD` on every keystroke that opens the palette.
//!
//! # What this module is, in three pieces
//!
//! 1. **The method** — [`list`], [`register_skill_methods`], the [`NAMESPACE`]/[`LIST_METHOD`]
//!    constants and the wire shape [`Entry`], matching upstream's `SkillEntry` field for field
//!    (`name`, optional `path`, `description`, optional `whenToUse`, `modelInvocable`).
//! 2. **The ranked catalog** — [`Source`], [`Candidate`], [`merge`] and [`entries`]. This is the
//!    part `skill_catalog` deliberately did not have: upstream ranks six roots
//!    (`project-dsh` 100 .. `bundled` 600) and the lowest rank wins, so a project's `review` skill
//!    shadows the bundled one of the same name. Rendering a prompt did not need that; serving a
//!    client that will *invoke* the name does.
//! 3. **The filesystem provider** — [`RootConfig`], [`roots`], [`discover`] and [`FilesystemHost`],
//!    which turn a session's working directory into the six roots upstream scans and read each
//!    skill's front matter into a [`Candidate`].
//!
//! # Why the session is resolved through a trait
//!
//! Upstream's `list` reads `sessionQuery.observeSession(sessionId)` for the `workingDirectory`
//! projection, falls back to `header.cwd`, and finally to `workingDirectory.defaultDirectory`; a
//! live agent's `ensure()` overrides all three. This crate's [`SessionCatalog`](girder_core::SessionCatalog)
//! returns [`SessionRecord`](girder_core::SessionRecord), which carries events but no header and no
//! projection map, so there is no honest way to read a session's directory from it here.
//! [`Sessions`] is that missing seam: a deployment that owns session state implements one method,
//! and [`FixedSessions`] is the in-memory form for a single-process host and for tests.
//!
//! The three-step fallback survives the seam. [`SessionScope::Known`] with no `cwd` means "the
//! session exists but recorded no directory", which is exactly upstream's `?? defaultDirectory`
//! branch, and [`Host::default_directory`] answers it. [`SessionScope::Missing`] is upstream's
//! `session/not-found`.
//!
//! # Why a missing session is an error rather than an empty catalog
//!
//! An empty `skills` array is what a project with no skills looks like. Answering that for a
//! mistyped id would tell the client "this session has no skills" when the true statement is
//! "there is no such session" — and the `/` palette would then show nothing at all, with no hint
//! that the id was wrong. [`RemoteError`] has no not-found variant, so this is `Failed` with the id
//! in the message, the same choice [`girder_core::session::resume_rpc`] and
//! [`girder_core::session::state_rpc`] make.
//!
//! # Why the response filters on `user_invocable`
//!
//! Upstream's method ends with `.filter(isUserInvocable)`. The catalog it serves is the *human*
//! palette, entered with `/name` in a composer. A skill whose author wrote
//! `user-invocable: false` is model-only on purpose; listing it would offer the person a command
//! that the invocation path then refuses. The model-facing filter (`modelInvocable`) is a separate
//! decision made by the `skill` tool, and the field is still reported so a client can say so.
//!
//! # Rank ties, and a deliberate divergence
//!
//! Upstream breaks a rank tie by provider *registration order*, then by position within the
//! provider. Registration order is a counter that resets on every restart, so two runs of the same
//! composition can disagree. This build breaks the tie by provider *name* first and keeps
//! registration order only across providers that share a name (which cannot happen — a registry
//! refuses duplicate names). Two deployments with the same files therefore rank identically, which
//! is the property a catalog digest depends on. The visible difference is confined to two providers
//! that both claim the same name at the same rank, which upstream documents as a warning case.
//!
//! # Blocking I/O inside an `async` method
//!
//! [`FilesystemHost::discover`] scans directories with `std::fs` inside the future it returns. That
//! blocks the executor thread for the length of one shallow directory read per root — six reads of
//! a directory that holds a handful of entries, on a method a client calls when the person types
//! `/`. The alternative was a `tokio::fs` round trip per entry, which costs more than it saves at
//! this size. The bound is worth stating because it is real: a skills root on a hung network mount
//! would park that thread, and a host with such a root should implement [`Host`] over its own
//! asynchronous filesystem service instead.

#![allow(dead_code)]

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use serde::Deserialize;
use serde_json::{Map, Value};

use girder_core::skill::policy::{
    frontmatter_optional_string, frontmatter_string, is_skill_name, parse_frontmatter_block,
    parse_invocation_policy,
};
use girder_core::{BoxFut, Declaration, RemoteError, RemoteRegistry};

/// The namespace `skills.list` lives under.
pub const NAMESPACE: &str = "skills";

/// The one method upstream defines in [`NAMESPACE`].
pub const LIST_METHOD: &str = "list";

/// Every method this module registers, for a manifest or a test to check against.
pub const METHODS: [&str; 1] = [LIST_METHOD];

/// The resource kind the access declaration names.
///
/// A read of the catalog, not of one skill: a policy that lets a peer list skills is not thereby
/// letting it read a skill's body, because no method here returns a body.
pub const RESOURCE_KIND: &str = "skills";

/// The file name of a directory-bundle skill, the same one [`girder_core::skill::catalog`] reads.
pub const SKILL_FILE: &str = "SKILL.md";

/// The provider name a [`FilesystemHost`] reports, upstream's `providerName` default.
pub const FILESYSTEM_PROVIDER: &str = "filesystem";

/// The project's own `.dsh/skills` root. Lowest rank: the most specific source wins.
pub const PROJECT_DSH_RANK: u32 = 100;

/// The project's shared `.agents/skills` root.
pub const PROJECT_AGENTS_RANK: u32 = 200;

/// A runtime-registered skill, between the project roots and the custom ones.
pub const RUNTIME_RANK: u32 = 250;

/// A caller-supplied root, after the project's own skills.
pub const CUSTOM_RANK: u32 = 300;

/// The user's `~/.girder/skills` root.
pub const USER_DSH_RANK: u32 = 400;

/// The user's shared `~/.agents/skills` root.
pub const USER_AGENTS_RANK: u32 = 500;

/// A bundled root shipped with the binary. Highest rank: anything of the same name outranks it.
pub const BUNDLED_RANK: u32 = 600;

/// Where a skill was discovered.
///
/// Upstream `SkillSource` also admits an arbitrary string (`| (string & {})`) because a third-party
/// provider may invent one. That openness is what a TypeScript union buys and what a Rust enum
/// cannot: a closed set here means an out-of-tree provider has to pick the nearest variant, and the
/// `custom` one exists for exactly that. Keeping it closed is the better trade because the variant
/// carries the rank, and an unknown string would have to carry a rank of its own anyway.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum Source {
    /// The project's `.dsh/skills`.
    ProjectDsh,
    /// The project's `.agents/skills`.
    ProjectAgents,
    /// Registered by the running process rather than discovered on disk.
    Runtime,
    /// A caller-supplied directory.
    Custom,
    /// The user's `~/.girder/skills`.
    UserDsh,
    /// The user's `~/.agents/skills`.
    UserAgents,
    /// Shipped with the binary.
    Bundled,
}

impl Source {
    /// The wire/prompt spelling upstream uses for this source.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::ProjectDsh => "project-dsh",
            Self::ProjectAgents => "project-agents",
            Self::Runtime => "runtime",
            Self::Custom => "custom",
            Self::UserDsh => "user-dsh",
            Self::UserAgents => "user-agents",
            Self::Bundled => "bundled",
        }
    }

    /// The precedence rank. **Lower wins** a collision.
    ///
    /// Lower-rather-than-higher is upstream's choice and it is the opposite of
    /// [`girder_core::skill::Provider::layer`], which is higher-wins. The two are not
    /// interchangeable and a caller must not sort one with the other's rule: upstream expresses
    /// "closer to the user wins" as a small number, the crate's registry expresses "later layer
    /// wins" as a large one.
    #[must_use]
    pub fn rank(self) -> u32 {
        match self {
            Self::ProjectDsh => PROJECT_DSH_RANK,
            Self::ProjectAgents => PROJECT_AGENTS_RANK,
            Self::Runtime => RUNTIME_RANK,
            Self::Custom => CUSTOM_RANK,
            Self::UserDsh => USER_DSH_RANK,
            Self::UserAgents => USER_AGENTS_RANK,
            Self::Bundled => BUNDLED_RANK,
        }
    }
}

/// One skill a provider offers, with everything the merge needs to rank it.
///
/// This is upstream's `SkillCandidate` minus the provider-owned `locator` and the free-form
/// `metadata`. Both are deliberately absent: `locator` is an opaque handle a provider passes back to
/// its own `get()`, and nothing in this module loads a body; `metadata` is provider-specific payload
/// no consumer of `skills.list` can interpret, and forwarding it would make this method's response
/// shape depend on which provider answered.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Candidate {
    /// The kebab-case identifier a consumer invokes.
    pub name: String,
    /// One line, shown in the palette.
    pub description: String,
    /// Extra routing guidance, when the skill supplied any.
    pub when_to_use: Option<String>,
    /// The absolute instruction file, when the skill exists on disk.
    pub path: Option<String>,
    /// Whether a model-facing catalog may include it.
    pub model_invocable: bool,
    /// Whether a human-facing command catalog may include it.
    pub user_invocable: bool,
    /// Where it came from.
    pub source: Source,
    /// Its precedence rank, normally `source.rank()`.
    pub rank: u32,
    /// The provider that reported it, for a diagnostic naming the winner.
    pub provider: String,
}

/// One entry of the `skills/list` response.
///
/// Mirrors upstream `SkillEntry` field for field. The optional two are *omitted* rather than sent as
/// `null`, which is what upstream's conditional spreads produce and what a client that distinguishes
/// "absent" from "null" needs.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Entry {
    /// The kebab-case identifier referenced as `/name`.
    pub name: String,
    /// Absolute `SKILL.md` path, when the provider supplied one.
    pub path: Option<String>,
    /// Short routing description.
    pub description: String,
    /// Optional extra routing guidance.
    pub when_to_use: Option<String>,
    /// Whether the same skill is also advertised to the model.
    pub model_invocable: bool,
}

impl Entry {
    /// The wire shape, matching upstream `SkillEntry`.
    #[must_use]
    pub fn to_json(&self) -> Value {
        let mut object = Map::new();
        object.insert(String::from("name"), Value::String(self.name.clone()));
        if let Some(path) = &self.path {
            object.insert(String::from("path"), Value::String(path.clone()));
        }
        object.insert(
            String::from("description"),
            Value::String(self.description.clone()),
        );
        if let Some(when_to_use) = &self.when_to_use {
            object.insert(
                String::from("whenToUse"),
                Value::String(when_to_use.clone()),
            );
        }
        object.insert(
            String::from("modelInvocable"),
            Value::Bool(self.model_invocable),
        );
        Value::Object(object)
    }
}

/// Merges every provider's candidates into the one winning catalog, sorted by name.
///
/// Three passes, in upstream's order:
///
/// 1. **Rank.** Ascending, so the lowest number wins. The sort is stable and the tie-break is the
///    provider name (see the module docs for why it is a name and not a registration counter).
/// 2. **First claim.** The first candidate for a name wins and every later one is dropped. This is
///    the same rule [`girder_core::skill::Registry::catalog`] applies to layers, and it is what
///    makes a project's `review` shadow a bundled `review` instead of producing two entries the
///    palette cannot tell apart.
/// 3. **Name order.** The result is sorted by name because a client renders it as a list and a
///    catalog that reordered between two calls would make the palette jump under the cursor.
///
/// Candidates arrive as one flat `Vec` rather than one per provider: the merge needs the rank and
/// the provider name, both of which are on the candidate, and a wrapper type would only restate
/// them.
#[must_use]
pub fn merge(candidates: &[Candidate]) -> Vec<Candidate> {
    let mut ordered: Vec<&Candidate> = candidates.iter().collect();
    ordered.sort_by(|left, right| {
        left.rank
            .cmp(&right.rank)
            .then_with(|| left.provider.cmp(&right.provider))
    });

    let mut claimed: BTreeSet<&str> = BTreeSet::new();
    let mut winners: Vec<Candidate> = Vec::new();
    for candidate in ordered {
        // `insert` returns false when the name was taken, which is the whole duplicate rule.
        if claimed.insert(candidate.name.as_str()) {
            winners.push(candidate.clone());
        }
    }
    winners.sort_by(|left, right| left.name.cmp(&right.name));
    winners
}

/// The response entries for a merged catalog: user-invocable skills only, in the given order.
///
/// Filtering happens here rather than in [`merge`] on purpose. The merge is the *catalog* — which
/// skill owns which name — and that answer is the same for every consumer. Which subset a consumer
/// may offer is the consumer's policy: the palette takes user-invocable skills, the `skill` tool
/// takes model-invocable ones, and a diagnostic listing takes both.
#[must_use]
pub fn entries(candidates: &[Candidate]) -> Vec<Entry> {
    candidates
        .iter()
        .filter(|candidate| candidate.user_invocable)
        .map(|candidate| Entry {
            name: candidate.name.clone(),
            path: candidate.path.clone(),
            description: candidate.description.clone(),
            when_to_use: candidate.when_to_use.clone(),
            model_invocable: candidate.model_invocable,
        })
        .collect()
}

/// The `skills/list` response value, upstream `SkillListValue`.
#[must_use]
pub fn list_value(entries: &[Entry]) -> Value {
    let skills: Vec<Value> = entries.iter().map(Entry::to_json).collect();
    let mut object = Map::new();
    object.insert(String::from("skills"), Value::Array(skills));
    Value::Object(object)
}

/// One directory to scan, and the source it speaks for.
///
/// The rank is derived from [`RootSpec::source`] rather than stored beside it. A root whose rank
/// disagreed with its source's rank would be a bug nothing could catch, and there is no case where a
/// caller wants a `project-dsh` root at `bundled` precedence.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RootSpec {
    /// The directory itself, not the skills inside it.
    pub path: PathBuf,
    /// Which source it stands for.
    pub source: Source,
    /// Whether a `.system` directory inside it is skipped.
    ///
    /// Upstream sets this only on the user's harness root, where `.system` is the harness's own
    /// bookkeeping rather than a skill. A project root keeps it visible so a person can still
    /// inspect it there.
    pub skip_system: bool,
}

impl RootSpec {
    /// Builds a root.
    #[must_use]
    pub fn new(path: impl Into<PathBuf>, source: Source) -> Self {
        Self {
            path: path.into(),
            source,
            skip_system: false,
        }
    }

    /// Marks the root as one whose `.system` entry is bookkeeping.
    #[must_use]
    pub fn skipping_system(mut self) -> Self {
        self.skip_system = true;
        self
    }
}

/// How to build the root list for one lookup.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct RootConfig {
    /// The directory whose project roots are scanned, when one is known.
    pub cwd: Option<PathBuf>,
    /// The harness home, `~/.girder` by default.
    pub dsh_home: Option<PathBuf>,
    /// The shared agents home, `~/.agents` by default.
    pub agents_home: Option<PathBuf>,
    /// Extra roots, scanned after the project's own and before the user's.
    pub custom: Vec<PathBuf>,
    /// Whether the project and user roots are included around the custom ones.
    ///
    /// `false` leaves only [`RootConfig::custom`] and [`RootConfig::bundled`], which is what a
    /// hermetic deployment wants: an operator who names their roots does not want a stray
    /// `~/.agents/skills` joining the catalog.
    pub include_defaults: bool,
    /// A bundled root, when the binary ships skills.
    pub bundled: Option<PathBuf>,
}

/// The nearest ancestor of `cwd` that holds a `.git` entry, or `cwd` itself.
///
/// Upstream's `findProjectRoot`. The fallback is the *original* directory rather than the filesystem
/// root: a session outside any repository still has its own project roots, and returning `/` would
/// scan the whole machine's `/SKILL.md`.
///
/// `.git` is checked with [`std::fs::metadata`], which follows symlinks, so a worktree whose `.git`
/// is a file (the `gitdir:` pointer form) counts — which is what makes a linked worktree a project.
///
/// The walk is unbounded upward, which is upstream's behaviour and is the point: a session opened in
/// `repo/crates/foo` should see the repository's skills. It terminates because each step's parent is
/// strictly shorter, and at the last step `parent() == None`.
#[must_use]
pub fn project_root(cwd: &Path) -> PathBuf {
    let mut current = cwd.to_path_buf();
    loop {
        if std::fs::metadata(current.join(".git")).is_ok() {
            return current;
        }
        match current.parent() {
            Some(parent) => current = parent.to_path_buf(),
            None => return cwd.to_path_buf(),
        }
    }
}

/// The roots to scan for one lookup, in precedence order.
///
/// Upstream's `FileSystemSkillProvider.roots`. The order is also the tie-break order the merge uses,
/// so it is not cosmetic: project before custom before user before bundled.
#[must_use]
pub fn roots(config: &RootConfig) -> Vec<RootSpec> {
    let mut roots: Vec<RootSpec> = Vec::new();

    if config.include_defaults
        && let Some(cwd) = &config.cwd
    {
        let project = project_root(cwd);
        roots.push(RootSpec::new(
            project.join(".dsh/skills"),
            Source::ProjectDsh,
        ));
        roots.push(RootSpec::new(
            project.join(".agents/skills"),
            Source::ProjectAgents,
        ));
    }

    for path in &config.custom {
        roots.push(RootSpec::new(path, Source::Custom));
    }

    if config.include_defaults {
        if let Some(home) = &config.dsh_home {
            roots.push(RootSpec::new(home.join("skills"), Source::UserDsh).skipping_system());
        }
        if let Some(home) = &config.agents_home {
            roots.push(RootSpec::new(home.join("skills"), Source::UserAgents));
        }
    }

    if let Some(bundled) = &config.bundled {
        roots.push(RootSpec::new(bundled, Source::Bundled));
    }

    roots
}

/// Reads one skill file into a candidate, or `None` when it is not one.
///
/// Every early return is upstream's: missing front matter, a name or description that is absent or
/// empty, a name outside [`is_skill_name`]'s grammar, and an invocation policy that cannot be read.
/// The body is not read past the front matter, for the reason
/// [`girder_core::skill::catalog`] documents — a catalog scan must not pay for every skill's
/// instructions.
///
/// The name grammar is enforced here, unlike in [`girder_core::parse_frontmatter`]. That parser
/// warns it is lax on purpose because it only feeds a prompt, and a prompt can show any string; this
/// catalog exists to be *invoked*, and a name a lookup cannot match would be offered and then never
/// resolve.
#[must_use]
pub fn parse_skill(text: &str, file: &Path, root: &RootSpec, provider: &str) -> Option<Candidate> {
    let frontmatter = parse_frontmatter_block(text)?;
    let name = frontmatter_string(&frontmatter.data, "name")?;
    if !is_skill_name(name) {
        return None;
    }
    let description = frontmatter_string(&frontmatter.data, "description")?;
    // A policy that cannot be read is a refusal, not a default: `user-invocable: maybe` would
    // otherwise silently mean "yes" on the one surface where the author meant to say something.
    let policy = parse_invocation_policy(&frontmatter.data).ok()?;
    Some(Candidate {
        name: name.to_owned(),
        description: description.to_owned(),
        when_to_use: frontmatter_optional_string(&frontmatter.data, "whenToUse").map(str::to_owned),
        path: Some(absolutize(file)),
        model_invocable: policy.model_invocable,
        user_invocable: policy.user_invocable,
        source: root.source,
        rank: root.source.rank(),
        provider: provider.to_owned(),
    })
}

/// Whether a root entry is a flat Markdown skill.
///
/// Case-**sensitive**, matching upstream's `entry.name.endsWith('.md')`: `README.MD` is not a skill
/// upstream and is not one here. Clippy's `case_sensitive_file_extension_comparisons` argues for the
/// insensitive form, which would accept files upstream ignores and change which skills exist, so the
/// comparison goes through [`std::path::Path::extension`] — exact, and the lint does not apply to it.
///
/// A leading-dot name such as `.md` has no extension and is skipped here; it is skipped a few lines
/// above anyway, as every dot entry is.
fn is_markdown(name: &str) -> bool {
    Path::new(name)
        .extension()
        .is_some_and(|extension| extension == "md")
}

/// Scans one root for skills.
///
/// Two shapes are accepted, both upstream's: a directory bundle (`<root>/<dir>/SKILL.md`) and a flat
/// Markdown file (`<root>/<name>.md`). A bucket of other files is skipped — a `README.md` *is*
/// accepted as a flat skill if it has front matter, which is upstream's behaviour and is why the
/// front-matter requirement is the real filter rather than the extension.
///
/// Entries are visited in byte order of the path so two runs agree, and a directory starting with a
/// dot is skipped unless it is `.system` in a root that does not skip `.system`: every other dot
/// entry a person puts in a skills shelf (`.git`, `.DS_Store`, `.vscode`) is tooling, and a
/// `.git/SKILL.md` would otherwise register as a skill named after a repository.
///
/// # Errors
///
/// None. A missing, unreadable, or non-UTF-8 entry yields no candidates. See the module docs: one bad
/// file must not empty the catalog, and `skills.list` has no per-file error channel to report it on.
#[must_use]
pub fn discover(root: &RootSpec, provider: &str) -> Vec<Candidate> {
    let Ok(read_dir) = std::fs::read_dir(&root.path) else {
        // A root that does not exist is the normal case for most projects, not a failure.
        return Vec::new();
    };
    let mut paths: Vec<PathBuf> = read_dir
        .filter_map(|entry| entry.ok().map(|entry| entry.path()))
        .collect();
    paths.sort();

    let mut found: Vec<Candidate> = Vec::new();
    for path in paths {
        let name = path
            .file_name()
            .map(|name| name.to_string_lossy().into_owned())
            .unwrap_or_default();
        if root.skip_system && name == ".system" {
            continue;
        }
        if name.starts_with('.') && name != ".system" {
            continue;
        }
        // `metadata` follows a symlink, so a shared bundle linked into a project root is a skill.
        let Ok(metadata) = std::fs::metadata(&path) else {
            continue;
        };
        let file = if metadata.is_dir() {
            path.join(SKILL_FILE)
        } else if metadata.is_file() && is_markdown(&name) {
            path
        } else {
            continue;
        };
        let Ok(text) = std::fs::read_to_string(&file) else {
            continue;
        };
        if let Some(candidate) = parse_skill(&text, &file, root, provider) {
            found.push(candidate);
        }
    }
    found
}

/// Scans every root, concatenating what each one found.
#[must_use]
pub fn discover_roots(roots: &[RootSpec], provider: &str) -> Vec<Candidate> {
    roots
        .iter()
        .flat_map(|root| discover(root, provider))
        .collect()
}

/// An absolute form of `path`.
///
/// [`std::fs::canonicalize`] is upstream's `realpath`, and it is what makes a symlinked root report
/// the file's real location. It fails on a path that vanished between the read and this call, which
/// is why the un-canonicalized join is the fallback: a path that is merely not canonical is still a
/// usable answer, and refusing to report the skill would lose it to a race.
fn absolutize(path: &Path) -> String {
    std::fs::canonicalize(path)
        .unwrap_or_else(|_| path.to_path_buf())
        .display()
        .to_string()
}

/// What `skills.list` learned about the session it addresses.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum SessionScope {
    /// The session exists, and this is its recorded working directory when it has one.
    ///
    /// `Known { cwd: None }` is upstream's `?? defaultDirectory` branch: a real session that never
    /// recorded a project directory still has a catalog, taken from the deployment's default.
    Known {
        /// The session's directory, when one was recorded.
        cwd: Option<String>,
    },
    /// No session has that id.
    Missing,
}

/// Where a session's working directory comes from.
///
/// The seam the module docs explain: this crate's [`SessionCatalog`](girder_core::SessionCatalog)
/// has no header and no projections, so a deployment supplies this instead. One method, because the
/// only question `skills.list` asks is where a session is standing — and whether it exists at all,
/// which [`SessionScope::Missing`] carries.
pub trait Sessions: Send + Sync {
    /// Resolves one session's scope.
    fn scope(&self, session_id: &str) -> BoxFut<SessionScope>;
}

/// A [`Sessions`] over a fixed table.
///
/// `None` for an id means "the session exists and recorded no directory"; an absent id means the
/// session does not exist. The distinction is the whole point of [`SessionScope`], so a map whose
/// values are `Option<String>` is the exact shape a host has.
#[derive(Clone, Debug, Default)]
pub struct FixedSessions {
    entries: std::collections::BTreeMap<String, Option<String>>,
}

impl FixedSessions {
    /// An empty table: every session id is unknown.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Records a session and its directory.
    #[must_use]
    pub fn with_session(mut self, session_id: impl Into<String>, cwd: Option<String>) -> Self {
        self.entries.insert(session_id.into(), cwd);
        self
    }
}

impl Sessions for FixedSessions {
    fn scope(&self, session_id: &str) -> BoxFut<SessionScope> {
        let scope = match self.entries.get(session_id) {
            Some(cwd) => SessionScope::Known { cwd: cwd.clone() },
            None => SessionScope::Missing,
        };
        Box::pin(async move { scope })
    }
}

/// What the `skills.list` method needs from the host.
///
/// Three methods rather than one, because they answer three separate questions and a deployment may
/// want to override one: where a session is standing ([`Host::scope`]), what to do when it recorded
/// nothing ([`Host::default_directory`]), and which skills exist there ([`Host::discover`]).
pub trait Host: Send + Sync {
    /// Resolves one session's scope.
    fn scope(&self, session_id: &str) -> BoxFut<SessionScope>;

    /// The directory used when a session recorded none.
    ///
    /// Absolute by contract: a relative default would resolve against whatever the process happened
    /// to start in, so two deployments would disagree about the same session.
    fn default_directory(&self) -> &str;

    /// Every skill visible from `cwd`, across every provider.
    fn discover(&self, cwd: Option<&str>) -> BoxFut<Vec<Candidate>>;
}

/// The filesystem host: ranked local roots, read from a session's directory.
#[derive(Clone)]
pub struct FilesystemHost {
    sessions: Arc<dyn Sessions>,
    default_directory: String,
    config: RootConfig,
}

impl std::fmt::Debug for FilesystemHost {
    /// Hand-written because [`Sessions`] is a trait object: deriving would require every
    /// implementation to be `Debug`, and a host that owns a live session store is the one type least
    /// likely to be. The directory and the root configuration are what a diagnostic needs.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("FilesystemHost")
            .field("default_directory", &self.default_directory)
            .field("config", &self.config)
            .finish_non_exhaustive()
    }
}

impl FilesystemHost {
    /// Builds a host over a session source and a default directory.
    #[must_use]
    pub fn new(sessions: Arc<dyn Sessions>, default_directory: impl Into<String>) -> Self {
        Self {
            sessions,
            default_directory: default_directory.into(),
            config: RootConfig {
                include_defaults: true,
                ..RootConfig::default()
            },
        }
    }

    /// Replaces the root configuration.
    ///
    /// The `cwd` on the supplied config is ignored: the working directory comes from the session the
    /// call addresses, and a fixed one here would answer every call with the same project's skills.
    #[must_use]
    pub fn with_roots(mut self, config: RootConfig) -> Self {
        self.config = RootConfig {
            cwd: None,
            ..config
        };
        self
    }

    /// The roots this host would scan from a working directory.
    #[must_use]
    pub fn roots_for(&self, cwd: &str) -> Vec<RootSpec> {
        let config = RootConfig {
            cwd: Some(PathBuf::from(cwd)),
            ..self.config.clone()
        };
        roots(&config)
    }
}

impl Host for FilesystemHost {
    fn scope(&self, session_id: &str) -> BoxFut<SessionScope> {
        self.sessions.scope(session_id)
    }

    fn default_directory(&self) -> &str {
        &self.default_directory
    }

    fn discover(&self, cwd: Option<&str>) -> BoxFut<Vec<Candidate>> {
        let config = RootConfig {
            cwd: cwd.map(PathBuf::from),
            ..self.config.clone()
        };
        Box::pin(async move { discover_roots(&roots(&config), FILESYSTEM_PROVIDER) })
    }
}

/// The request to `skills.list`, upstream `SkillListRequest`.
#[derive(Debug, Default, Deserialize)]
#[serde(default)]
struct ListRequest {
    /// Session whose composition selects the catalog view.
    #[serde(rename = "sessionId")]
    session_id: String,
}

/// The session id a `skills.list` call addresses.
///
/// Two spellings are accepted for the same reason [`girder_core::api::api_final::session_id_of`]
/// accepts two: a transport may forward the `{request: {...}}` envelope verbatim or unwrap it before
/// dispatch, and this crate's other methods show both shapes in the wild. A client that used the
/// wrong one would otherwise be told its session does not exist rather than that its field was
/// nested one level off.
///
/// # Errors
///
/// [`RemoteError::InvalidArguments`] when neither spelling yields a non-empty id, and when the id is
/// not a string at all.
pub fn session_id_of(args: &Value) -> Result<String, RemoteError> {
    let request = args
        .get("request")
        .filter(|value| value.is_object())
        .unwrap_or(args);
    let parsed: ListRequest =
        serde_json::from_value(request.clone()).map_err(|error| RemoteError::InvalidArguments {
            namespace: NAMESPACE.to_owned(),
            method: LIST_METHOD.to_owned(),
            message: error.to_string(),
        })?;
    if parsed.session_id.trim().is_empty() {
        return Err(RemoteError::InvalidArguments {
            namespace: NAMESPACE.to_owned(),
            method: LIST_METHOD.to_owned(),
            message: String::from(
                "`sessionId` is required: the catalog is addressed by the session whose working \
                 directory selects it",
            ),
        });
    }
    Ok(parsed.session_id)
}

/// Runs one `skills.list` call.
///
/// The order is upstream's: resolve the session, take its directory (or the default), discover,
/// merge, filter to user-invocable entries. A failure at the first step is reported and the rest is
/// not attempted, so a bad id costs one lookup rather than six directory scans.
///
/// # Errors
///
/// [`RemoteError::InvalidArguments`] for a missing id, and [`RemoteError::Failed`] when no session
/// has the requested id.
pub async fn list(args: &Value, host: &dyn Host) -> Result<Value, RemoteError> {
    let session_id = session_id_of(args)?;
    let cwd = match host.scope(&session_id).await {
        SessionScope::Missing => {
            return Err(RemoteError::Failed(format!(
                "no session `{session_id}` is known to this deployment"
            )));
        }
        // A blank recorded directory is treated as none, which is the same reading
        // `girder_core::util::paths` applies to a blank environment override: empty is not a path.
        SessionScope::Known { cwd: Some(cwd) } if !cwd.trim().is_empty() => cwd,
        SessionScope::Known { .. } => host.default_directory().to_owned(),
    };
    let candidates = host.discover(Some(&cwd)).await;
    Ok(list_value(&entries(&merge(&candidates))))
}

/// Registers `skills.list`.
///
/// Declared a read of [`RESOURCE_KIND`]: the method touches two directories and no state. A mirror
/// that may show a session must be able to offer its `/` palette, and declaring a write would refuse
/// it the one call that makes the palette work.
pub fn register_skill_methods(registry: &mut RemoteRegistry, host: Arc<dyn Host>) {
    registry.declare(NAMESPACE, LIST_METHOD, Declaration::read(RESOURCE_KIND));
    registry.register(NAMESPACE, LIST_METHOD, move |call| {
        let host = Arc::clone(&host);
        Box::pin(async move { list(&call.args, host.as_ref()).await })
    });
}
