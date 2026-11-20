//! Plugin admission and package logistics: the decisions upstream `packages/boot` makes *before* a
//! package manager or a Node loader runs.
//!
//! # What upstream `packages/boot` contains, measured
//!
//! Six packages, 57 production sources, 13,301 lines (test and `.d.ts` files excluded). Grouped by
//! the decision each family makes, with the Cordis column read off whether the files import
//! `@deepseek-ai/cordis` or hang an effect on a `Context`:
//!
//! | Family | Files | Lines | Cordis | What it decides |
//! | --- | --- | --- | --- | --- |
//! | `plugin-manager` service host | `index.ts`, `operations.ts`, `tools.ts` | 2,317 | yes | The `PluginManager` service, `pnpm` invocations, build approval, Host RPC |
//! | `plugin-market` | `index.ts`, `repository-cache.ts`, `detail.ts`, `search.ts`, `parse*.ts`, `index-url.ts`, `types.ts`, … | 1,858 | partly (`tools.ts` only) | Market index parsing, search, repository detail fetching |
//! | `hmr` | 5 files | 1,740 | yes | Module invalidation and reload over a live plugin tree |
//! | `app-boot` launcher | `index.ts`, `startup-diagnostics.ts`, `compatibility-preflight.ts`, `profile-context.ts` | 1,662 | yes | Capability capture, fail-loud interception, mount order |
//! | `app-boot` profile resolution | `profile-resolution/*` | 1,265 | partly | Node ESM/CJS loader interception (1,072 of the lines) |
//! | `app-boot` profile | `profile.ts`, `profile-plugins.ts`, `package-meta.ts`, `profile-sanitize.ts` | 1,129 | no | Profile discovery, bundle layers, patch composition |
//! | `app-boot` config schema | `config-schema/*` | 1,082 | partly | Schemastery graph → JSON Schema projection (797 lines are boot-free) |
//! | `plugin-manager` logistics core | `install-spec.ts`, `registry.ts`, `install-failure.ts`, `versions.ts`, `official-install-target.ts`, `diagnostic.ts`, `failure.ts`, `types.ts` | 1,031 | no | Spec parsing, registry order, failure reading, published versions, record validation |
//! | `plugin-manager` process integration | `patch.ts`, `run-tree.ts`, `github-connection.ts`, `bundle-detail.ts`, `build-approval.ts`, `git-transport.ts` | 529 | no | Child-process supervision, Git transport choice, bundle documents |
//! | `app-boot` admission | `plugin-compatibility.ts`, `profile-compatibility.ts`, `bundle-platform.ts` | 293 | no | `engines.dsh`/peer requirements, exact-version exemptions, host admission |
//! | `cmdline` | 1 file | 239 | yes | Launcher → app argument hand-off |
//! | `config-editor` | 1 file | 156 | yes | A live config document service |
//!
//! 4,435 of those lines import nothing from `@deepseek-ai/cordis`; the remaining 8,866 lines do.
//! "Cordis" is the useful split here because this crate has no Cordis `Context`: anything that
//! exists to publish a service, inject a dependency, or hang an effect on a fiber has no local
//! counterpart to grow from, while the decisions upstream keeps outside Cordis are ordinary pure
//! functions that a Rust build needs for exactly the same reasons.
//!
//! # What this build already covers
//!
//! `boot.rs` (387 lines) mounts *components* — provider, tools, sessions, event bus — and
//! `boot_check.rs` (201) reports deployment findings; `boot_stages.rs` (336) traces stage order.
//! Those answer "did the application come up", which upstream answers in `app-boot/src/index.ts`
//! through Cordis. `plugin_lifecycle.rs`, `extensions.rs`, `fiber.rs` and `registry.rs` are the
//! Cordis-shaped half. A symbol-by-symbol grep finds **no** local counterpart for
//! `parseInstallSpec`, `pinnedGitOrigin`, `dependencySpec`, `newerVersions`,
//! `evaluatePluginCompatibility`, `normalizeRegistry`, `registryPlan`, `classifyInstallFailure`,
//! `readProfileCompatibility`, `isExactPluginVersion`, `assertBundlePlatform`,
//! `officialBundleInstallTarget`, `normalizeIndexUrl`, `parseIndexBody`, `loadProfile` or
//! `bundlePatchPaths`: zero hits, across the whole workspace.
//!
//! # The gaps, largest first, split by Cordis
//!
//! **Cordis.** `plugin-manager`'s service host (2,317), `hmr` (1,740) and `app-boot`'s launcher
//! (1,662) are runtime services over a plugin tree this build does not have: a `PluginManager`
//! that publishes itself, a module invalidator wired to a loader, a mount sequence over fibers.
//! They cannot be ported as a module, and `fiber.rs`, `extensions.rs`, `plugin_lifecycle.rs` and
//! `registry.rs` are this build's own answer to the same problem in its own vocabulary.
//!
//! **Non-Cordis**, by size:
//!
//! 1. `plugin-market` (1,858 lines, 1,717 of them outside Cordis). Nominally the largest, but the
//!    bulk is I/O it delegates: `repository-cache.ts` drives `git` subprocesses,
//!    `detail.ts` leans on `hosted-git-info`, `search.ts` is a `MiniSearch` integration, and
//!    `parse.ts` is a 43-line wrapper around an external `parseMarketIndex`. What is this
//!    package's own decision — index-source URL validation — is 26 lines.
//! 2. `app-boot` profile (1,129). Bundle composition over `cordis.patch.yml` lists in Cordis's own
//!    include dialect: composing those in Rust would invent an incompatible dialect rather than
//!    port one, so only the bundle *declaration* half is portable, and that is here.
//! 3. `plugin-manager` logistics core (1,031) and admission (293). **1,324 lines of pure decision
//!    logic** — the largest family that is decisions rather than I/O or dialect, and the one
//!    implemented here.
//!
//! # Why this family, and not the nominally larger one
//!
//! The size order and the portability order disagree, and portability is what decides whether a
//! port is *correct*. A Rust `MiniSearch` would not be `MiniSearch`; a Rust `cordis.patch.yml`
//! composer would produce patches no Cordis loader accepts. An install spec, a registry plan, and
//! an exemption record are the same values in any language, which is why they are the ones ported.
//!
//! # What is deliberately absent
//!
//! - Semver *ranges* (`evaluatePluginCompatibility`'s `satisfies`). [`SemVer`] parses and orders
//!   versions, which is what [`is_exact_plugin_version`] and [`newer_versions`] need. It does not
//!   evaluate ranges: a partial range evaluator reports compatible plugins as incompatible, and
//!   because `engines.dsh` decides alone, a wrong answer is not a degraded answer but a refused
//!   install.
//! - The `pnpm view` and `git ls-remote` subprocesses, the profile's file lock, and the atomic
//!   write a grant performs. [`update_profile_compatibility`] returns the document to write and
//!   leaves writing it, under the lock, to the caller that owns the profile directory.
//!
//! # What is implemented
//!
//! | Upstream symbol | Here |
//! | --- | --- |
//! | `parseInstallSpec` (`install-spec.ts`) | [`parse_install_spec`] |
//! | `ParsedInstallSpec` | [`ParsedSpec`], [`SpecKind`] |
//! | `InvalidInstallSpecError` | [`InvalidSpec`] |
//! | `pinnedGitOrigin` | [`pinned_git_origin`] |
//! | `dependencySpec` | [`dependency_spec`] |
//! | `normalizeRegistry`, `REGISTRY_URL` | [`normalize_registry`], [`is_registry_url`] |
//! | `registryPlan`, `PluginRegistries`, `Registry` | [`registry_plan`], [`Registries`], [`Registry`] |
//! | `OFFICIAL_NPM_REGISTRY`, `NPMMIRROR_REGISTRY` | the same two constants |
//! | `attributeFailure` | [`attribute_failure`] |
//! | `classifyInstallFailure`, `InstallFailureFacts` | [`classify_install_failure`] |
//! | `isExactPluginVersion` (semver `parse`) | [`SemVer`], [`is_exact_plugin_version`] |
//! | `readProfileCompatibility`, `readProfileVersionExemptions` | [`read_profile_compatibility`] |
//! | `validatePluginVersionExemption` | [`validate_plugin_version_exemption`] |
//! | `setProfileVersionExemption` | [`update_profile_compatibility`] + [`render_compatibility`] |
//! | `assertBundlePlatform`, `UnsupportedBundlePlatformError`, `isWsl` | [`assert_bundle_platform`], [`UnsupportedPlatform`], [`is_wsl`] |
//! | `newerVersions` (`versions.ts`) | [`newer_versions`] |
//!
//! What is *not* here from those same files is the part that is not a decision: `pnpm view`'s
//! subprocess call, `git ls-remote` probing, file locking, and the atomic write a grant performs.
//! [`update_profile_compatibility`] therefore returns the document to write and leaves writing it,
//! under the profile's own lock, to the caller that owns the profile directory.
//!
//! # Why every entry point takes its facts instead of reading them
//!
//! `parse_install_spec` takes the typed spec, not `process.argv`; `read_profile_compatibility`
//! reads one named file, not `$DSH_HOME`; [`is_wsl`] takes the environment facts instead of
//! calling `std::env::var`. A check that reads the world answers differently on a different machine
//! and cannot be tested at all, and these checks decide whether a plugin is allowed to run.
//!
//! # Why the crate root is spelled `girder_core`
//!
//! This module is not declared in `lib.rs` yet — wiring is a separate change. The test compiles
//! this exact source by `#[path]`, and under that inclusion `crate::` names the test binary, so
//! every path out of the module would have to go through the crate root's re-exports. Nothing here
//! needs one, so the module imports only `std` and its two already-declared dependencies.

#![allow(
    dead_code,
    reason = "this module is compiled privately by the `#[path]` test until lib.rs re-exports it; every item here is public surface for that wiring, so 'unused within this test binary' is not dead code"
)]

use std::collections::BTreeMap;
use std::fmt::Write as _;
use std::path::{Component, Path};

use serde_json::Value;

// ============================================================================================
// Install specs — upstream `plugin-manager/src/install-spec.ts`
// ============================================================================================

/// The maximum length npm accepts for a package name (`PACKAGE_NAME_MAX_LENGTH`).
const PACKAGE_NAME_MAX_LENGTH: usize = 214;

/// Which of `pnpm`'s spec forms a spec takes — upstream `ParsedInstallSpec['kind']`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum SpecKind {
    /// A package name, with an optional range after `@`.
    Registry,
    /// An absolute path to a package directory.
    Path,
    /// A tarball on disk or over HTTP.
    Tarball,
    /// A Git repository.
    Git,
}

impl SpecKind {
    /// The name this kind is reported under, in `pnpm`'s vocabulary.
    #[must_use]
    pub const fn label(self) -> &'static str {
        match self {
            Self::Registry => "registry",
            Self::Path => "path",
            Self::Tarball => "tarball",
            Self::Git => "git",
        }
    }
}

/// One install spec read into its form and the parts an inspection needs — upstream
/// `ParsedInstallSpec`.
///
/// A Git spec and a tarball URL carry the `host` `pnpm` fetches them from, which no registry stands
/// in for; only their dependencies come from the registry. That distinction is the whole point of
/// [`attribute_failure`]: a failure that names the spec's own host cannot be fixed by asking another
/// registry.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ParsedSpec {
    /// A registry name and the range that follows it, if any.
    Registry {
        /// The spec as typed, trimmed.
        spec: String,
        /// The package name, without its range.
        name: String,
        /// The range, tag, or alias after `@`.
        range: Option<String>,
    },
    /// An absolute package directory on disk.
    Path {
        /// The spec as typed, trimmed.
        spec: String,
        /// The absolute directory, with any `file:` or `link:` prefix removed.
        path: String,
    },
    /// A tarball, on disk or over HTTP.
    Tarball {
        /// The spec as typed, trimmed.
        spec: String,
        /// The absolute path, when the tarball is on disk.
        path: Option<String>,
        /// The host the tarball is fetched from, when it is a URL.
        host: Option<String>,
    },
    /// A Git repository.
    Git {
        /// The spec as typed, trimmed.
        spec: String,
        /// The lower-case host (and port) the repository is cloned from.
        host: String,
    },
}

impl ParsedSpec {
    /// The spec as typed, trimmed.
    #[must_use]
    pub fn spec(&self) -> &str {
        match self {
            Self::Registry { spec, .. }
            | Self::Path { spec, .. }
            | Self::Tarball { spec, .. }
            | Self::Git { spec, .. } => spec,
        }
    }

    /// Which form the spec takes.
    #[must_use]
    pub const fn kind(&self) -> SpecKind {
        match self {
            Self::Registry { .. } => SpecKind::Registry,
            Self::Path { .. } => SpecKind::Path,
            Self::Tarball { .. } => SpecKind::Tarball,
            Self::Git { .. } => SpecKind::Git,
        }
    }

    /// The host a Git spec or a tarball URL is fetched from, which no registry stands in for.
    ///
    /// `None` for a registry name or a local path: those are resolved by the registry or the file
    /// system, and blaming a host they do not name is the mistake [`attribute_failure`] exists to
    /// avoid.
    #[must_use]
    pub fn host(&self) -> Option<&str> {
        match self {
            Self::Git { host, .. } => Some(host),
            Self::Tarball { host, .. } => host.as_deref(),
            Self::Registry { .. } | Self::Path { .. } => None,
        }
    }
}

/// A spec neither `pnpm` nor the registry would take — upstream `InvalidInstallSpecError`.
///
/// Upstream throws, not returns, so this type is the only difference in shape at the call site:
/// [`parse_install_spec`] returns `Err(Self)` where upstream throws.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
#[error("plugin-manager: {reason}: {spec}")]
pub struct InvalidSpec {
    /// The spec as typed, trimmed.
    pub spec: String,
    /// Why it is refused, as one sentence.
    pub reason: String,
}

impl InvalidSpec {
    fn new(spec: &str, reason: &str) -> Self {
        Self {
            spec: spec.to_owned(),
            reason: reason.to_owned(),
        }
    }
}

/// Whether a path is absolute, as the spec reader's caller means it.
///
/// POSIX `/x`, a Windows drive path `C:\x` or `C:/x`, and a UNC `\\server\share` are absolute;
/// everything else resolves against whatever directory the process happened to start in, which is
/// meaningless to the person typing an install spec into a browser.
#[must_use]
pub fn is_absolute_path(path: &str) -> bool {
    if path.starts_with('/') || path.starts_with("\\\\") {
        return true;
    }
    let bytes = path.as_bytes();
    bytes.len() >= 3
        && bytes[0].is_ascii_alphabetic()
        && bytes[1] == b':'
        && (bytes[2] == b'/' || bytes[2] == b'\\')
}

/// Strip a prefix, ignoring ASCII case.
///
/// Compared as bytes and sliced only on the match, so a multi-byte character straddling the prefix
/// length cannot panic the way `text[..len]` would.
fn strip_prefix_ascii_ci<'a>(text: &'a str, prefix: &str) -> Option<&'a str> {
    let head = text.as_bytes().get(..prefix.len())?;
    if head.eq_ignore_ascii_case(prefix.as_bytes()) {
        Some(&text[prefix.len()..])
    } else {
        None
    }
}

/// Whether the spec names a tarball (`TARBALL_SPEC`): `.tgz` or `.tar.gz`, with an optional
/// fragment after `#`.
///
/// Sliced and lower-cased rather than asked of `Path::extension`, because `.tar.gz` is two
/// extensions and a URL's query string is not a file name.
#[allow(
    clippy::case_sensitive_file_extension_comparisons,
    reason = "the comparison is case-insensitive: both sides are lower-cased first"
)]
fn is_tarball_spec(spec: &str) -> bool {
    let without_fragment = spec.split_once('#').map_or(spec, |(before, _)| before);
    let lower = without_fragment.to_ascii_lowercase();
    lower.ends_with(".tgz") || lower.ends_with(".tar.gz")
}

/// The host a `github:`-style shorthand stands for (`GIT_SHORTHAND_HOSTS`).
fn shorthand_host(name: &str) -> Option<&'static str> {
    match name {
        "github" => Some("github.com"),
        "gitlab" => Some("gitlab.com"),
        "bitbucket" => Some("bitbucket.org"),
        "gist" => Some("gist.github.com"),
        _ => None,
    }
}

/// The shorthand prefix of a spec (`GIT_SHORTHAND`): the name before `:`, when it is one `pnpm`
/// knows.
fn git_shorthand(spec: &str) -> Option<&'static str> {
    let (name, _) = spec.split_once(':')?;
    if name.is_empty() || !name.bytes().all(|byte| byte.is_ascii_alphabetic()) {
        return None;
    }
    shorthand_host(&name.to_ascii_lowercase())
}

/// Whether the spec is a Git URL (`GIT_URL`): a `git://`-family URL, or an scp-like
/// `git@host:path`.
fn is_git_url(spec: &str) -> bool {
    if strip_prefix_ascii_ci(spec, "git://").is_some() {
        return true;
    }
    if let Some(rest) = strip_prefix_ascii_ci(spec, "git+") {
        let letters = rest.bytes().take_while(u8::is_ascii_lowercase).count();
        if letters > 0 && rest[letters..].starts_with("://") {
            return true;
        }
    }
    if let Some(rest) = strip_prefix_ascii_ci(spec, "git@") {
        // `^git@[^:]+:` — a non-empty host, then the scp separator.
        if let Some(colon) = rest.find(':') {
            return colon > 0;
        }
    }
    false
}

/// Whether the spec is a hosted repository page (`HOSTED_REPOSITORY_URL`):
/// `https://host/owner/project`, optionally `.git` and a fragment.
///
/// Exactly two path segments, because that is the shape a forge serves a repository at: a deeper
/// path such as `/owner/project/tree/main/pkg` is a page *inside* a repository, not a repository,
/// and treating it as one makes `git clone` fail with a message about a directory it invented.
fn is_hosted_repository_url(spec: &str) -> bool {
    let rest =
        strip_prefix_ascii_ci(spec, "https://").or_else(|| strip_prefix_ascii_ci(spec, "http://"));
    let Some(rest) = rest else {
        return false;
    };
    let Some((host, path)) = rest.split_once('/') else {
        return false;
    };
    if host.is_empty() {
        return false;
    }
    let mut segments = path.split('/');
    let (Some(owner), Some(project), None) = (segments.next(), segments.next(), segments.next())
    else {
        return false;
    };
    if owner.is_empty() {
        return false;
    }
    let name = project
        .split_once('#')
        .map_or(project, |(before, _)| before);
    !name.is_empty() && !name.contains('#')
}

/// The host a Git spec is cloned from (`gitHost`): the shorthand's host, the scp-like
/// `user@host`, or the URL's host with its port.
fn git_host(spec: &str) -> String {
    if let Some(host) = git_shorthand(spec) {
        return host.to_owned();
    }
    if let Some(rest) = strip_prefix_ascii_ci(spec, "git@")
        && let Some((host, _)) = rest.split_once(':')
        && !host.is_empty()
    {
        return host.to_ascii_lowercase();
    }
    let without_prefix = strip_prefix_ascii_ci(spec, "git+").unwrap_or(spec);
    let authority = without_prefix
        .split_once("://")
        .map_or(without_prefix, |(_, rest)| rest);
    let authority = authority.split(['/', '?', '#']).next().unwrap_or(authority);
    // Credentials in a URL are not the host; the host begins after the last `@`.
    let host = authority
        .rsplit_once('@')
        .map_or(authority, |(_, host)| host);
    host.to_ascii_lowercase()
}

/// Whether `name` is a package name the registry accepts (`PACKAGE_NAME`): lower-case URL-safe
/// segments, an optional scope, and no leading dot or underscore.
#[must_use]
pub fn is_registry_package_name(name: &str) -> bool {
    let (scope, body) = match name.strip_prefix('@') {
        Some(rest) => match rest.split_once('/') {
            Some((scope, body)) => (Some(scope), body),
            // A scope without its name is not a package name.
            None => return false,
        },
        None => (None, name),
    };
    if let Some(scope) = scope
        && !is_name_segment(scope)
    {
        return false;
    }
    is_name_segment(body)
}

/// One `PACKAGE_NAME` segment: a lower-case URL-safe run that does not start with `.` or `_`.
fn is_name_segment(segment: &str) -> bool {
    let mut bytes = segment.bytes();
    let Some(first) = bytes.next() else {
        return false;
    };
    if !(first.is_ascii_lowercase() || first.is_ascii_digit()) {
        return false;
    }
    bytes.all(|byte| {
        byte.is_ascii_lowercase()
            || byte.is_ascii_digit()
            || matches!(byte, b'.' | b'_' | b'~' | b'-')
    })
}

/// Read a spec into its form — upstream `parseInstallSpec`.
///
/// A path must be absolute: the Host's working directory means nothing to the person typing into a
/// browser, and a relative path resolved against the profile would point inside it.
///
/// # Errors
///
/// Returns [`InvalidSpec`] for an empty spec, a relative path, a name the registry would refuse, or
/// a URL that is neither a Git host nor a tarball.
pub fn parse_install_spec(raw: &str) -> Result<ParsedSpec, InvalidSpec> {
    let spec = raw.trim();
    if spec.is_empty() {
        return Err(InvalidSpec::new(spec, "the package spec must not be empty"));
    }

    // A `file:` or `link:` prefix names a local path; so does a bare absolute path. Both are
    // answered here so that a tarball on disk is reported as a tarball and a directory as a path.
    let path = spec
        .strip_prefix("file:")
        .or_else(|| spec.strip_prefix("link:"))
        .unwrap_or(spec);
    if path != spec || is_absolute_path(path) {
        if !is_absolute_path(path) {
            return Err(InvalidSpec::new(spec, "a local path must be absolute"));
        }
        return Ok(if is_tarball_spec(path) {
            ParsedSpec::Tarball {
                spec: spec.to_owned(),
                path: Some(path.to_owned()),
                host: None,
            }
        } else {
            ParsedSpec::Path {
                spec: spec.to_owned(),
                path: path.to_owned(),
            }
        });
    }

    // `./x` and `../x` are local paths the person forgot to make absolute. `.hidden` is not: it is a
    // name the registry refuses, and saying so names the real problem.
    if starts_with_dot_path(spec) {
        return Err(InvalidSpec::new(spec, "a local path must be absolute"));
    }

    // A Git host: the three address forms `pnpm` resolves through one, unless the address is a
    // tarball URL, which is a download rather than a clone.
    if (git_shorthand(spec).is_some() || is_git_url(spec) || is_hosted_repository_url(spec))
        && !is_tarball_spec(spec)
    {
        return Ok(ParsedSpec::Git {
            host: git_host(spec),
            spec: spec.to_owned(),
        });
    }

    if strip_prefix_ascii_ci(spec, "https://").is_some()
        || strip_prefix_ascii_ci(spec, "http://").is_some()
    {
        if is_tarball_spec(spec) {
            return Ok(ParsedSpec::Tarball {
                spec: spec.to_owned(),
                path: None,
                host: Some(git_host(spec)),
            });
        }
        return Err(InvalidSpec::new(
            spec,
            "a URL must point at a git repository or a tarball",
        ));
    }

    // A registry name: the `@` that starts a range is never the first character, because the first
    // one of a scoped name is part of the name.
    // A range begins at the first `@` after the first character: the leading one of a scoped name
    // is part of the name. Byte offsets come from `char_indices`, so a multi-byte first character
    // cannot put the slice inside itself.
    let range_start = spec
        .char_indices()
        .skip(1)
        .find(|(_, character)| *character == '@')
        .map(|(index, _)| index);
    let (name, range) = match range_start {
        Some(at) => (&spec[..at], Some(spec[at + 1..].to_owned())),
        None => (spec, None),
    };
    if name.len() > PACKAGE_NAME_MAX_LENGTH || !is_registry_package_name(name) {
        return Err(InvalidSpec::new(
            spec,
            "not a package name the registry accepts",
        ));
    }
    if range.as_deref() == Some("") {
        return Err(InvalidSpec::new(
            spec,
            "a version after @ must not be empty",
        ));
    }
    Ok(ParsedSpec::Registry {
        spec: spec.to_owned(),
        name: name.to_owned(),
        range,
    })
}

/// Whether the spec begins with `.` or `..` followed by a separator or nothing (`^\.{1,2}(?:[\\/]|$)`).
fn starts_with_dot_path(spec: &str) -> bool {
    let rest = spec.strip_prefix('.').unwrap_or(spec);
    let rest = rest.strip_prefix('.').unwrap_or(rest);
    rest.is_empty() || rest.starts_with('/') || rest.starts_with('\\')
}

/// A full SHA-1 commit name, as a fragment parameter of a Git spec (`FULL_COMMIT`).
fn is_full_commit(text: &str) -> bool {
    text.len() == 40 && text.bytes().all(|byte| byte.is_ascii_hexdigit())
}

/// Where a Git spec pinned to a full commit installs from — upstream `pinnedGitOrigin`.
///
/// The origin is the lower-case host and the repository path, without transport, user, or `.git`,
/// then `#` and the `path:` directory. Equal origins name one repository on a host that serves it
/// at the same path over every transport, as Git forges do; on a plain SSH server, an scp-like path
/// is relative to the login's home directory, which is why this comparison only claims what the
/// form makes true.
///
/// Returns `None` for a spec that is not Git, or whose fragment is not one full commit and at most
/// one `path:` directory.
#[must_use]
pub fn pinned_git_origin(spec: &str) -> Option<String> {
    let parsed = parse_install_spec(spec).ok()?;
    if parsed.kind() != SpecKind::Git {
        return None;
    }
    let spec = parsed.spec();
    let (location, fragment) = spec.split_once('#')?;
    let parameters: Vec<&str> = fragment.split('&').collect();
    let paths: Vec<&str> = parameters
        .iter()
        .copied()
        .filter(|part| part.starts_with("path:"))
        .collect();
    let others: Vec<&str> = parameters
        .iter()
        .copied()
        .filter(|part| !part.starts_with("path:"))
        .collect();
    // `pnpm` reads the last of repeated parameters, so a fragment that repeats one names no single
    // origin.
    if paths.len() > 1 || others.len() != 1 || !is_full_commit(others[0]) {
        return None;
    }
    let directory = paths
        .first()
        .map_or("", |part| part["path:".len()..].trim_matches('/'));
    let location = normalize_git_location(location);
    Some(format!("{location}#{directory}"))
}

/// Strip transport, user, and `.git` from a Git location, lower-casing its host half.
fn normalize_git_location(location: &str) -> String {
    let mut text = location.to_owned();
    for (shorthand, host) in [
        ("github:", "github.com/"),
        ("gitlab:", "gitlab.com/"),
        ("bitbucket:", "bitbucket.org/"),
        ("gist:", "gist.github.com/"),
    ] {
        if let Some(rest) = strip_prefix_ascii_ci(&text, shorthand) {
            text = format!("{host}{rest}");
            break;
        }
    }
    let text = strip_prefix_ascii_ci(&text, "git+")
        .unwrap_or(&text)
        .to_owned();
    // Drop the transport scheme: `ssh://`, `https://`, `git://`, and friends.
    let text = match text.split_once("://") {
        Some((scheme, rest)) if !scheme.contains('/') => rest.to_owned(),
        _ => text,
    };
    // Drop the user information of an scp-like or URL form.
    let text = match text.split_once('@') {
        Some((user, rest)) if !user.contains('/') => rest.to_owned(),
        _ => text,
    };
    // The scp-like `host:path` separator, but not the colon of a port number.
    let text = scp_separator_to_slash(&text);
    let text = text.trim_end_matches('/').to_owned();
    let text = match strip_suffix_ascii_ci(&text, ".git") {
        Some(rest) => rest.to_owned(),
        None => text,
    };
    match text.split_once('/') {
        Some((host, rest)) => format!("{}/{rest}", host.to_ascii_lowercase()),
        None => text.to_ascii_lowercase(),
    }
}

/// Replace the first `host:path` separator with `/`, when the colon is not followed by a digit.
///
/// The digit test is what keeps a port a port: `host:2222/a/b` has no path separator before the
/// port, and turning it into `host/2222/a/b` would name a different repository path.
fn scp_separator_to_slash(text: &str) -> String {
    let Some(colon) = text.find(':') else {
        return text.to_owned();
    };
    // A port follows a digit; a path does not.
    let followed_by_digit = text[colon + 1..]
        .chars()
        .next()
        .is_some_and(|next| next.is_ascii_digit());
    if followed_by_digit {
        return text.to_owned();
    }
    format!("{}/{}", &text[..colon], &text[colon + 1..])
}

/// Strip a suffix, ignoring ASCII case.
fn strip_suffix_ascii_ci<'a>(text: &'a str, suffix: &str) -> Option<&'a str> {
    let start = text.len().checked_sub(suffix.len())?;
    if text.as_bytes()[start..].eq_ignore_ascii_case(suffix.as_bytes()) {
        Some(&text[..start])
    } else {
        None
    }
}

/// The spec `pnpm add` accepts for a dependency the profile manifest records — upstream
/// `dependencySpec`.
///
/// A Git address, URL, or other non-registry protocol is the recorded value without the user
/// information of an HTTP(S) URL, whose query string is kept; a `file:` or `link:` path, which
/// `pnpm` records relative to the profile or with `~` for the home directory, becomes absolute. A
/// registry range, tag, or alias follows the dependency name after `@`, as does a non-registry
/// value installed under a name other than the package's own.
///
/// `home` is passed in rather than read: a test that read the real `HOME` could not state the case
/// it is describing.
#[must_use]
pub fn dependency_spec(
    name: &str,
    recorded: &str,
    profile_dir: &Path,
    home: &Path,
    package_name: &str,
) -> String {
    let local = recorded
        .strip_prefix("file:")
        .map(|path| ("file:", path))
        .or_else(|| recorded.strip_prefix("link:").map(|path| ("link:", path)));
    let spec = if let Some((prefix, path)) = local {
        let path = expand_home(path, home);
        let resolved = resolve_against(profile_dir, &path);
        format!("{prefix}{}", resolved.display())
    } else if is_non_registry_value(recorded) {
        strip_http_user_info(recorded)
    } else {
        return format!("{name}@{recorded}");
    };
    if package_name == name {
        spec
    } else {
        format!("{name}@{spec}")
    }
}

/// Replace a leading `~` (alone, or before a separator) with the home directory.
fn expand_home(path: &str, home: &Path) -> String {
    let rest = if path == "~" {
        Some("")
    } else {
        path.strip_prefix("~/").or_else(|| path.strip_prefix("~\\"))
    };
    match rest {
        Some(rest) => home.join(rest).display().to_string(),
        None => path.to_owned(),
    }
}

/// Resolve `path` against `base`, folding `.` and `..` lexically.
///
/// Lexical rather than canonical: the profile directory may not exist yet when a manifest is read,
/// and a resolution that failed there would report every fresh profile as broken.
fn resolve_against(base: &Path, path: &str) -> std::path::PathBuf {
    let combined = if Path::new(path).is_absolute() {
        std::path::PathBuf::from(path)
    } else {
        base.join(path)
    };
    let mut out = std::path::PathBuf::new();
    for component in combined.components() {
        match component {
            Component::CurDir => {}
            Component::ParentDir => {
                // `..` at the root stays at the root, as a filesystem resolve does.
                if !out.pop() {
                    out.push("/");
                }
            }
            other => out.push(other.as_os_str()),
        }
    }
    out
}

/// A recorded dependency value `pnpm` resolves outside a registry (`NON_REGISTRY_VALUE`): a
/// protocol other than a registry alias, or an scp-like git address.
fn is_non_registry_value(recorded: &str) -> bool {
    let mut bytes = recorded.bytes();
    if let Some(first) = bytes.next() {
        let mut scheme = Vec::new();
        if first.is_ascii_lowercase() {
            scheme.push(first);
            for byte in bytes {
                if byte.is_ascii_lowercase()
                    || byte.is_ascii_digit()
                    || matches!(byte, b'+' | b'.' | b'-')
                {
                    scheme.push(byte);
                } else {
                    if byte == b':' {
                        let scheme = String::from_utf8_lossy(&scheme).to_ascii_lowercase();
                        return !matches!(scheme.as_str(), "npm" | "jsr" | "workspace" | "catalog");
                    }
                    break;
                }
            }
        }
    }
    // `^[^@/:\s]+@[^:/\s]+:` — the scp-like `host:path` form, which has no scheme.
    let Some((user, rest)) = recorded.split_once('@') else {
        return false;
    };
    if user.is_empty()
        || user.contains('/')
        || user.contains(':')
        || user.chars().any(char::is_whitespace)
    {
        return false;
    }
    let Some((host, _)) = rest.split_once(':') else {
        return false;
    };
    !host.is_empty()
        && !host.contains('/')
        && !host.chars().any(char::is_whitespace)
        && !user.is_empty()
}

/// The user information of an HTTP(S) URL, up to the last `@` before the path, which holds a
/// password or access token when present (`HTTP_USER_INFO`).
fn strip_http_user_info(recorded: &str) -> String {
    let (prefix, rest) = if let Some(rest) = strip_prefix_ascii_ci(recorded, "git+https://") {
        ("git+https://", rest)
    } else if let Some(rest) = strip_prefix_ascii_ci(recorded, "git+http://") {
        ("git+http://", rest)
    } else if let Some(rest) = strip_prefix_ascii_ci(recorded, "https://") {
        ("https://", rest)
    } else if let Some(rest) = strip_prefix_ascii_ci(recorded, "http://") {
        ("http://", rest)
    } else {
        return recorded.to_owned();
    };
    let authority = rest.split('/').next().unwrap_or(rest);
    match authority.rsplit_once('@') {
        Some((_, host)) => format!("{prefix}{host}{}", &rest[authority.len()..]),
        None => recorded.to_owned(),
    }
}

// ============================================================================================
// Registries — upstream `plugin-manager/src/registry.ts`
// ============================================================================================

/// npm's own registry: what `pnpm` names without any configuration (`OFFICIAL_NPM_REGISTRY`).
pub const OFFICIAL_NPM_REGISTRY: &str = "https://registry.npmjs.org/";

/// The public npmmirror URL shared by the fallback configuration (`NPMMIRROR_REGISTRY`).
pub const NPMMIRROR_REGISTRY: &str = "https://registry.npmmirror.com/";

/// A registry a package operation asks (`Registry`): a URL, or `None` for the one `pnpm`'s own
/// configuration names.
pub type Registry = Option<String>;

/// Whether `text` is an HTTP(S) URL, as `pnpm`'s `--registry` takes it (`REGISTRY_URL`).
#[must_use]
pub fn is_registry_url(text: &str) -> bool {
    (text.starts_with("http://") || text.starts_with("https://"))
        && text.len() > "https://".len()
        && !text.chars().any(char::is_whitespace)
}

/// The registries one operation asks — upstream `PluginRegistries`.
#[allow(
    clippy::struct_field_names,
    reason = "the field name mirrors upstream's `fallbackRegistries`; renaming it here would make the upstream mapping harder to follow"
)]
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Registries {
    /// The configured first registry; `None` for the one `pnpm`'s own configuration names.
    pub registry: Registry,
    /// The fallbacks, asked after the first one.
    pub fallback_registries: Vec<String>,
    /// The URL `pnpm`'s own configuration names in the profile; `None` when it could not be read.
    pub resolved: Option<String>,
}

/// The `requested` argument of [`registry_plan`].
///
/// Upstream distinguishes `undefined` (defer to the configured first registry) from `null` (ask the
/// one `pnpm` names), so the three cases are distinct here rather than collapsed into
/// `Option<Registry>`, where `Some(None)` would be unreadable at every call site.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub enum RegistryRequest {
    /// The caller named no registry.
    #[default]
    Configured,
    /// The caller asked for the registry `pnpm`'s own configuration names.
    PnpmOwn,
    /// The caller asked for this URL.
    Url(String),
}

/// A registry that is not an HTTP(S) URL.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
#[error("a registry must be an http(s) URL: {url}")]
pub struct RegistryError {
    /// The registry as configured or requested.
    pub url: String,
}

/// Parse a registry URL into the form `pnpm` compares registries in: lower-case host, trailing
/// slash.
///
/// # Errors
///
/// Returns [`RegistryError`] for anything but an absolute HTTP(S) URL.
pub fn normalize_registry(url: &str) -> Result<String, RegistryError> {
    let refuse = || RegistryError {
        url: url.to_owned(),
    };
    let (scheme, rest) = url.split_once("://").ok_or_else(refuse)?;
    let scheme = scheme.to_ascii_lowercase();
    if scheme != "http" && scheme != "https" {
        return Err(refuse());
    }
    let authority_end = rest.find(['/', '?', '#']).unwrap_or(rest.len());
    let (authority, tail) = rest.split_at(authority_end);
    if authority.is_empty() {
        return Err(refuse());
    }
    // Lower-case the host, which begins after the user information when there is any.
    let (user_info, host) = match authority.rsplit_once('@') {
        Some((user_info, host)) => (Some(user_info), host),
        None => (None, authority),
    };
    if host.is_empty() || host.contains(char::is_whitespace) {
        return Err(refuse());
    }
    let host = host.to_ascii_lowercase();
    let mut normalized = match user_info {
        Some(user_info) => format!("{scheme}://{user_info}@{host}"),
        None => format!("{scheme}://{host}"),
    };
    // Split the tail into its path and the query/fragment that follows it, so the trailing slash
    // lands on the path rather than after the query.
    let (path, suffix) = match tail.find(['?', '#']) {
        Some(index) => tail.split_at(index),
        None => (tail, ""),
    };
    normalized.push_str(path);
    if !path.ends_with('/') {
        normalized.push('/');
    }
    normalized.push_str(suffix);
    Ok(normalized)
}

/// The registries one operation asks, first to last — upstream `registryPlan`.
///
/// A requested registry is asked first when it is one of the configured set, alone otherwise, so a
/// private registry never falls through to a public one. `pnpm`'s own registry belongs to the set
/// only when what it names is known to be public; while it names anything else, or is unknown, it is
/// asked alone, and a public registry asked instead never falls back into it.
///
/// # Errors
///
/// Returns [`RegistryError`] when a configured or requested registry is not an HTTP(S) URL: the plan
/// cannot order what it cannot compare.
pub fn registry_plan(
    requested: &RegistryRequest,
    configured: &Registries,
) -> Result<Vec<Registry>, RegistryError> {
    let own = configured
        .resolved
        .as_deref()
        .map(normalize_registry)
        .transpose()?;
    let fallbacks = configured
        .fallback_registries
        .iter()
        .map(String::as_str)
        .map(normalize_registry)
        .collect::<Result<Vec<_>, _>>()?;
    let own_is_public = own
        .as_deref()
        .is_some_and(|own| own == OFFICIAL_NPM_REGISTRY || fallbacks.iter().any(|url| url == own));

    // What a registry is compared as: `pnpm`'s own registry stands for the URL it names, once that
    // is known.
    let key_of = |registry: &Registry| -> Result<Option<String>, RegistryError> {
        match registry {
            None => Ok(own.clone()),
            Some(url) => normalize_registry(url).map(Some),
        }
    };

    let mut known: Vec<Registry> = Vec::new();
    let mut keys: Vec<Option<String>> = Vec::new();
    let mut configured_order: Vec<Registry> = Vec::with_capacity(1 + fallbacks.len());
    configured_order.push(configured.registry.clone());
    for fallback in &configured.fallback_registries {
        configured_order.push(Some(fallback.clone()));
    }
    for registry in &configured_order {
        if registry.is_none() && !own_is_public {
            continue;
        }
        let key = key_of(registry)?;
        if keys.contains(&key) {
            continue;
        }
        let normalized = match registry {
            None => None,
            Some(url) => Some(normalize_registry(url)?),
        };
        known.push(normalized);
        keys.push(key);
    }

    let first: Registry = match requested {
        RegistryRequest::Configured => configured.registry.clone(),
        RegistryRequest::PnpmOwn => None,
        RegistryRequest::Url(url) => Some(url.clone()),
    };
    let first_key = key_of(&first)?;
    let normalized_first = match &first {
        None => None,
        Some(url) => Some(normalize_registry(url)?),
    };
    // A private or unknown registry of `pnpm`'s own, whichever way it was asked for, is asked
    // alone: no public fallback may stand in for it.
    if !own_is_public && (first.is_none() || first_key == own) {
        return Ok(vec![normalized_first]);
    }
    if !keys.contains(&first_key) {
        return Ok(vec![normalized_first]);
    }
    let mut plan = vec![normalized_first];
    for (index, registry) in known.into_iter().enumerate() {
        if keys[index] != first_key {
            plan.push(registry);
        }
    }
    Ok(plan)
}

/// What a failed attempt could not reach or get an answer from — upstream `attributeFailure`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FailedAt {
    /// The registry: another one, or the same one later, can answer differently.
    Registry,
    /// The host a Git or tarball spec is fetched from, which no registry stands in for.
    SpecHost,
    /// Neither, because the failure is not one another registry could change.
    Other,
}

impl FailedAt {
    /// The name this attribution is reported under.
    #[must_use]
    pub const fn label(self) -> &'static str {
        match self {
            Self::Registry => "registry",
            Self::SpecHost => "spec-host",
            Self::Other => "other",
        }
    }
}

/// The failures after which another registry can answer differently (`NEXT_REGISTRY_KINDS`): this
/// one was unreachable, or its copy may be stale.
fn is_next_registry_kind(kind: InstallFailureKind) -> bool {
    matches!(
        kind,
        InstallFailureKind::Network
            | InstallFailureKind::Timeout
            | InstallFailureKind::NotFound
            | InstallFailureKind::NoMatchingVersion
    )
}

/// Attribute a failed package attempt — upstream `attributeFailure`.
///
/// The question this answers is whether asking a *different* registry could help. A registry's own
/// unreachability is retryable elsewhere; a Git host that cannot be reached is not, and blaming the
/// registry for it sends the caller through a fallback list that never had a chance.
#[must_use]
pub fn attribute_failure(kind: InstallFailureKind, log: &str, spec: &ParsedSpec) -> FailedAt {
    if !is_next_registry_kind(kind) {
        return FailedAt::Other;
    }
    let Some(host) = spec.host() else {
        return FailedAt::Registry;
    };
    let host = host.to_ascii_lowercase();
    let named = log
        .split('\n')
        .any(|line| is_error_line(line) && line.to_ascii_lowercase().contains(&host));
    if named {
        FailedAt::SpecHost
    } else {
        FailedAt::Registry
    }
}

/// A line of `pnpm` or `git` output that reports a failure, as opposed to a warning or progress
/// line (`ERROR_LINE`).
fn is_error_line(line: &str) -> bool {
    line.contains("ERR_")
        || line.contains("ERROR")
        || contains_word(line, "error")
        || line.contains("fatal:")
        || line.contains("Could not resolve")
        || line.contains("unable to access")
        || line.contains("ssh:")
        || contains_errno_word(line)
}

/// `\bE[A-Z]{4,}\b`: an errno-style constant such as `ENOSPC` or `ETIMEDOUT`.
fn contains_errno_word(line: &str) -> bool {
    let bytes = line.as_bytes();
    for start in 0..bytes.len() {
        if bytes[start] != b'E' || (start > 0 && is_ascii_word_byte(bytes[start - 1])) {
            continue;
        }
        let caps = bytes[start..]
            .iter()
            .skip(1)
            .take_while(|byte| byte.is_ascii_uppercase())
            .count();
        if caps >= 4 {
            let end = start + 1 + caps;
            if end == bytes.len() || !is_ascii_word_byte(bytes[end]) {
                return true;
            }
        }
    }
    false
}

/// Whether a byte continues a JavaScript `\w` word: ASCII letters, digits, and `_`.
fn is_ascii_word_byte(byte: u8) -> bool {
    byte.is_ascii_alphanumeric() || byte == b'_'
}

/// Whether `haystack` contains `needle` bounded by non-word bytes, as JavaScript's `\b` does.
///
/// ASCII-only on purpose: `\b` in JavaScript is defined over `[A-Za-z0-9_]`, so a CJK character
/// beside `ECONNRESET` *is* a boundary and the errno name still matches inside Chinese output.
fn contains_word(haystack: &str, needle: &str) -> bool {
    if needle.is_empty() {
        return true;
    }
    let haystack = haystack.as_bytes();
    let needle = needle.as_bytes();
    let mut start = 0;
    while let Some(offset) = find_bytes(&haystack[start..], needle) {
        let at = start + offset;
        let before_ok = at == 0 || !is_ascii_word_byte(haystack[at - 1]);
        let end = at + needle.len();
        let after_ok = end == haystack.len() || !is_ascii_word_byte(haystack[end]);
        if before_ok && after_ok {
            return true;
        }
        start = at + 1;
    }
    false
}

/// Find `needle` in `haystack`, byte-wise.
fn find_bytes(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    if needle.is_empty() || needle.len() > haystack.len() {
        return None;
    }
    haystack
        .windows(needle.len())
        .position(|window| window == needle)
}

// ============================================================================================
// Failure reading — upstream `plugin-manager/src/install-failure.ts`
// ============================================================================================

/// How a package operation failed, read off how it ended and what it printed — upstream
/// `PluginInstallFailureKind`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum InstallFailureKind {
    /// The package manager itself is not installed.
    PnpmMissing,
    /// The run outlived its bound.
    Timeout,
    /// The registry does not have the package, or that version of it.
    NotFound,
    /// The registry has the package but no version matches.
    NoMatchingVersion,
    /// The registry, or the spec's host, could not be reached.
    Network,
    /// The disk is full.
    DiskFull,
    /// A file could not be read or written.
    Permission,
    /// A build script was blocked by policy, so the install is incomplete.
    BuildBlocked,
    /// A tarball did not match its recorded integrity.
    Integrity,
    /// Nothing in the facts names a cause.
    Unknown,
}

impl InstallFailureKind {
    /// The name this kind is reported under.
    #[must_use]
    pub const fn label(self) -> &'static str {
        match self {
            Self::PnpmMissing => "pnpm-missing",
            Self::Timeout => "timeout",
            Self::NotFound => "not-found",
            Self::NoMatchingVersion => "no-matching-version",
            Self::Network => "network",
            Self::DiskFull => "disk-full",
            Self::Permission => "permission",
            Self::BuildBlocked => "build-blocked",
            Self::Integrity => "integrity",
            Self::Unknown => "unknown",
        }
    }
}

/// How a run ended and what it printed — upstream `InstallFailureFacts`.
#[derive(Clone, Copy, Debug, Default)]
pub struct InstallFailureFacts<'a> {
    /// The output tail the failure reports.
    pub log: &'a str,
    /// The spawn error's code, when the child never ran.
    pub cause_code: Option<&'a str>,
    /// Whether the run outlived its bound.
    pub timed_out: bool,
}

/// Classify a failed run — upstream `classifyInstallFailure`.
///
/// `pnpm` names its failures with stable `ERR_PNPM_*` codes and Node's errno names, which the run's
/// captured tail carries whatever the locale, so the tail is read and the exit code is not: a
/// non-zero exit that printed nothing recognizable is [`InstallFailureKind::Unknown`], which is the
/// honest answer and the one that stops a fallback from being attempted for the wrong reason.
#[must_use]
pub fn classify_install_failure(facts: &InstallFailureFacts<'_>) -> InstallFailureKind {
    if facts.timed_out {
        return InstallFailureKind::Timeout;
    }
    if facts.cause_code == Some("ENOENT") {
        return InstallFailureKind::PnpmMissing;
    }
    let log = facts.log;
    // Ordered: a specific code is read before the generic network family it also matches.
    if log.contains("ERR_PNPM_IGNORED_BUILDS")
        || log.contains("Ignored build scripts")
        || log.contains("ERR_PNPM_GIT_DEP_PREPARE_NOT_ALLOWED")
    {
        return InstallFailureKind::BuildBlocked;
    }
    if log.contains("ERR_PNPM_FETCH_404")
        || contains_word(log, "E404")
        || log.contains("404 Not Found")
        || log.contains("Not Found - GET")
    {
        return InstallFailureKind::NotFound;
    }
    if log.contains("ERR_PNPM_NO_MATCHING_VERSION")
        || contains_word(log, "ETARGET")
        || log.contains("No matching version")
    {
        return InstallFailureKind::NoMatchingVersion;
    }
    if contains_word(log, "ENOSPC") || contains_ignore_ascii_case(log, "no space left on device") {
        return InstallFailureKind::DiskFull;
    }
    if contains_word(log, "EACCES")
        || contains_word(log, "EPERM")
        || contains_ignore_ascii_case(log, "permission denied")
    {
        return InstallFailureKind::Permission;
    }
    if log.contains("ERR_PNPM_TARBALL_INTEGRITY")
        || log.contains("ERR_PNPM_BAD_TARBALL_SIZE")
        || contains_word(log, "EINTEGRITY")
    {
        return InstallFailureKind::Integrity;
    }
    if names_forbidden_http_status(log) {
        return InstallFailureKind::Unknown;
    }
    if contains_word(log, "ENOTFOUND")
        || contains_word(log, "ECONNRESET")
        || contains_word(log, "ETIMEDOUT")
        || contains_word(log, "ECONNREFUSED")
        || contains_word(log, "EAI_AGAIN")
        || log.contains("ERR_PNPM_META_FETCH_FAIL")
        || log.contains("ERR_PNPM_FETCH_TIMEOUT")
        || contains_word(log, "FETCH_ERROR")
        || log.contains("socket hang up")
        || log.contains("Could not resolve host")
        || log.contains("unable to access")
        || names_pnpm_fetch_5xx(log)
    {
        return InstallFailureKind::Network;
    }
    InstallFailureKind::Unknown
}

/// `The requested URL returned error: 40[134]\b`: Git's own 401/403/404 report, which is a
/// credential or repository problem rather than a registry one, so it is [`InstallFailureKind::Unknown`]
/// and never triggers a registry fallback.
fn names_forbidden_http_status(log: &str) -> bool {
    let marker = "The requested URL returned error: 40";
    let bytes = log.as_bytes();
    let mut start = 0;
    while let Some(offset) = log[start..].find(marker) {
        let after = start + offset + marker.len();
        if let Some(status) = bytes.get(after)
            && matches!(status, b'1' | b'3' | b'4')
            && (after + 1 == bytes.len() || !is_ascii_word_byte(bytes[after + 1]))
        {
            return true;
        }
        start = after;
    }
    false
}

/// `ERR_PNPM_FETCH_5\d\d`: a gateway or server failure on the metadata fetch.
fn names_pnpm_fetch_5xx(log: &str) -> bool {
    let marker = "ERR_PNPM_FETCH_5";
    let bytes = log.as_bytes();
    let mut start = 0;
    while let Some(offset) = log[start..].find(marker) {
        let after = start + offset + marker.len();
        if let (Some(first), Some(second)) = (bytes.get(after), bytes.get(after + 1))
            && first.is_ascii_digit()
            && second.is_ascii_digit()
        {
            return true;
        }
        start = after;
    }
    false
}

/// Whether `haystack` contains `needle`, ignoring ASCII case.
fn contains_ignore_ascii_case(haystack: &str, needle: &str) -> bool {
    haystack
        .to_ascii_lowercase()
        .contains(&needle.to_ascii_lowercase())
}

// ============================================================================================
// Versions and exemptions — upstream `app-boot/src/profile-compatibility.ts`,
// `plugin-manager/src/versions.ts`
// ============================================================================================

/// One prerelease identifier, which compares numerically when it is a number.
#[derive(Clone, Debug, PartialEq, Eq)]
enum Identifier {
    Numeric(u64),
    Alpha(String),
}

/// A semantic version, parsed and ordered by semver precedence.
///
/// This is the piece of `node-semver` that [`is_exact_plugin_version`] and [`newer_versions`] need:
/// a strict parse that preserves the exact spelling, and the precedence ordering. It is **not** a
/// range evaluator; see the module documentation for why a partial one would be worse than none.
///
/// Equality here follows semver *precedence*, which ignores build metadata: `1.0.0+a` and `1.0.0+b`
/// compare equal, exactly as `semver.eq` reports them.
#[derive(Clone, Debug)]
pub struct SemVer {
    major: u64,
    minor: u64,
    patch: u64,
    prerelease: Vec<Identifier>,
    build: Vec<String>,
}

impl SemVer {
    /// Parse a version, tolerating surrounding whitespace and a single leading `v`, as
    /// `node-semver`'s parser does.
    ///
    /// Returns `None` for anything that is not `major.minor.patch`, with optional prerelease and
    /// build metadata, and for a numeric field with a leading zero.
    #[must_use]
    pub fn parse(text: &str) -> Option<Self> {
        let text = text.trim();
        let text = text.strip_prefix('v').unwrap_or(text);
        let (text, build) = match text.split_once('+') {
            Some((core, build)) => (core, split_identifiers(build)?),
            None => (text, Vec::new()),
        };
        let (core, prerelease) = match text.split_once('-') {
            Some((core, prerelease)) => (core, split_identifiers(prerelease)?),
            None => (text, Vec::new()),
        };
        let mut fields = core.split('.');
        let major = numeric_field(fields.next()?)?;
        let minor = numeric_field(fields.next()?)?;
        let patch = numeric_field(fields.next()?)?;
        if fields.next().is_some() {
            return None;
        }
        let prerelease = prerelease
            .into_iter()
            .map(|identifier| {
                if identifier.bytes().all(|byte| byte.is_ascii_digit()) {
                    // Leading zeros are refused: `01` is not a number semver accepts.
                    if identifier.len() > 1 && identifier.starts_with('0') {
                        return None;
                    }
                    identifier.parse().ok().map(Identifier::Numeric)
                } else {
                    Some(Identifier::Alpha(identifier))
                }
            })
            .collect::<Option<Vec<_>>>()?;
        Some(Self {
            major,
            minor,
            patch,
            prerelease,
            build,
        })
    }

    /// Whether this version is a prerelease.
    #[must_use]
    pub fn is_prerelease(&self) -> bool {
        !self.prerelease.is_empty()
    }

    /// The build metadata, in declaration order.
    #[must_use]
    pub fn build(&self) -> &[String] {
        &self.build
    }
}

/// Split a dotted identifier list, refusing an empty identifier or a character semver excludes.
fn split_identifiers(text: &str) -> Option<Vec<String>> {
    let identifiers: Vec<String> = text.split('.').map(str::to_owned).collect();
    if identifiers.is_empty() {
        return None;
    }
    for identifier in &identifiers {
        if identifier.is_empty() {
            return None;
        }
        if !identifier
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-')
        {
            return None;
        }
    }
    Some(identifiers)
}

/// Parse a numeric field, refusing a leading zero (`01`).
fn numeric_field(text: &str) -> Option<u64> {
    if text.is_empty() || !text.bytes().all(|byte| byte.is_ascii_digit()) {
        return None;
    }
    if text.len() > 1 && text.starts_with('0') {
        return None;
    }
    text.parse().ok()
}

impl std::fmt::Display for SemVer {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}.{}.{}", self.major, self.minor, self.patch)?;
        if !self.prerelease.is_empty() {
            f.write_str("-")?;
            for (index, identifier) in self.prerelease.iter().enumerate() {
                if index > 0 {
                    f.write_str(".")?;
                }
                match identifier {
                    Identifier::Numeric(value) => write!(f, "{value}")?,
                    Identifier::Alpha(text) => f.write_str(text)?,
                }
            }
        }
        if !self.build.is_empty() {
            f.write_str("+")?;
            f.write_str(&self.build.join("."))?;
        }
        Ok(())
    }
}

impl PartialEq for SemVer {
    fn eq(&self, other: &Self) -> bool {
        self.cmp(other) == std::cmp::Ordering::Equal
    }
}

impl Eq for SemVer {}

impl PartialOrd for SemVer {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        Some(self.cmp(other))
    }
}

impl Ord for SemVer {
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        self.major
            .cmp(&other.major)
            .then_with(|| self.minor.cmp(&other.minor))
            .then_with(|| self.patch.cmp(&other.patch))
            .then_with(|| compare_prerelease(&self.prerelease, &other.prerelease))
    }
}

/// Compare prerelease identifiers: an absent prerelease sorts *after* a present one, numeric
/// identifiers compare numerically and sort before alphanumeric ones, and a shorter run sorts
/// before a longer one that starts with it.
fn compare_prerelease(left: &[Identifier], right: &[Identifier]) -> std::cmp::Ordering {
    use std::cmp::Ordering;
    match (left.is_empty(), right.is_empty()) {
        (true, true) => return Ordering::Equal,
        (true, false) => return Ordering::Greater,
        (false, true) => return Ordering::Less,
        (false, false) => {}
    }
    for (left, right) in left.iter().zip(right.iter()) {
        let ordering = match (left, right) {
            (Identifier::Numeric(left), Identifier::Numeric(right)) => left.cmp(right),
            (Identifier::Numeric(_), Identifier::Alpha(_)) => Ordering::Less,
            (Identifier::Alpha(_), Identifier::Numeric(_)) => Ordering::Greater,
            (Identifier::Alpha(left), Identifier::Alpha(right)) => left.cmp(right),
        };
        if ordering != Ordering::Equal {
            return ordering;
        }
    }
    left.len().cmp(&right.len())
}

/// Whether `value` is a canonical exact version, including build metadata — upstream
/// `isExactPluginVersion`.
///
/// A range, a prefix, surrounding whitespace, or a `v` prefix is refused: an exemption that named
/// `^1.2` or `v1.2.3` would compare unequal to every version it was meant to allow, and a grant
/// that silently allows nothing is worse than a refusal that says so.
#[must_use]
pub fn is_exact_plugin_version(value: &str) -> bool {
    SemVer::parse(value).is_some_and(|parsed| parsed.to_string() == value)
}

/// A package name npm accepts in a manifest dependency key (`PACKAGE_NAME`, without the URL-safe
/// `~` this reader refuses).
fn is_exemption_package_name(name: &str) -> bool {
    let (scope, body) = match name.strip_prefix('@') {
        Some(rest) => match rest.split_once('/') {
            Some(parts) => (Some(parts.0), parts.1),
            None => return false,
        },
        None => (None, name),
    };
    let segment_ok = |segment: &str| -> bool {
        let mut bytes = segment.bytes();
        let Some(first) = bytes.next() else {
            return false;
        };
        if !(first.is_ascii_lowercase() || first.is_ascii_digit()) {
            return false;
        }
        bytes.all(|byte| {
            byte.is_ascii_lowercase() || byte.is_ascii_digit() || matches!(byte, b'.' | b'_' | b'-')
        })
    };
    scope.is_none_or(segment_ok) && segment_ok(body)
}

/// Split `package@version` at its last `@`.
fn split_package_version(key: &str) -> Option<(&str, &str)> {
    let separator = key.rfind('@')?;
    if separator == 0 {
        return None;
    }
    Some((&key[..separator], &key[separator + 1..]))
}

/// What one profile's compatibility file currently authorizes, and what is wrong with it —
/// upstream `ProfileCompatibility`.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct CompatibilityFile {
    /// Accepted exact `package-name@version` keys mapped to their allowed DSH versions.
    pub exemptions: BTreeMap<String, Vec<String>>,
    /// Human-readable problems; empty when every record was accepted.
    pub warnings: Vec<String>,
    /// Whether the file holds nothing this reader rejected, so a grant or revocation may rewrite it.
    ///
    /// A false value means writing would discard content the user must repair by hand, which is why
    /// [`update_profile_compatibility`] refuses rather than repairs.
    pub rewritable: bool,
}

/// The profile's own compatibility file (`PROFILE_COMPATIBILITY_FILENAME`).
pub const PROFILE_COMPATIBILITY_FILENAME: &str = "compatibility.json";

/// Read the profile's independent compatibility file without loading plugins — upstream
/// `readProfileCompatibility`.
///
/// A missing file authorizes nothing and is reported as nothing: a fresh profile is not a broken
/// one. An unreadable or unparsable file authorizes nothing and is *reported* instead of failing,
/// so a bad file can never make the profile unusable; rejected records are skipped while the
/// remaining valid ones still apply.
#[must_use]
pub fn read_profile_compatibility(profile_dir: &Path) -> CompatibilityFile {
    let filename = profile_dir.join(PROFILE_COMPATIBILITY_FILENAME);
    let display = filename.display().to_string();
    if !filename.exists() {
        return CompatibilityFile {
            exemptions: BTreeMap::new(),
            warnings: Vec::new(),
            rewritable: true,
        };
    }
    match std::fs::read_to_string(&filename) {
        Ok(text) => parse_compatibility(&display, Some(&text)),
        Err(error) => unreadable(&display, &error.to_string()),
    }
}

/// A file that cannot be read authorizes nothing and must not be rewritten.
fn unreadable(filename: &str, reason: &str) -> CompatibilityFile {
    CompatibilityFile {
        exemptions: BTreeMap::new(),
        warnings: vec![format!(
            "{filename} cannot be read ({reason}); treating the profile as having no exemptions"
        )],
        rewritable: false,
    }
}

/// Parse the compatibility document — the pure half of [`read_profile_compatibility`].
///
/// `text` is `None` for a missing file, which authorizes nothing and is rewritable. Taking the text
/// rather than a path is what makes every rejection case testable without a filesystem.
#[must_use]
pub fn parse_compatibility(filename: &str, text: Option<&str>) -> CompatibilityFile {
    let Some(text) = text else {
        return CompatibilityFile {
            exemptions: BTreeMap::new(),
            warnings: Vec::new(),
            rewritable: true,
        };
    };
    let illegal = |reason: &str| CompatibilityFile {
        exemptions: BTreeMap::new(),
        warnings: vec![format!(
            "{filename} {reason}; treating the profile as having no exemptions"
        )],
        rewritable: false,
    };
    let value: Value = match serde_json::from_str(text) {
        Ok(value) => value,
        Err(error) => return illegal(&format!("is not valid JSON ({error})")),
    };
    let Some(document) = value.as_object() else {
        return illegal("must map exact package@version keys to DSH version lists");
    };
    let mut exemptions = BTreeMap::new();
    let mut warnings = Vec::new();
    for (key, versions) in document {
        let accepted = split_package_version(key).is_some_and(|(name, version)| {
            is_exemption_package_name(name) && is_exact_plugin_version(version)
        });
        if !accepted {
            warnings.push(format!(
                "{filename}: {key:?} is not an exact package-name@version key; the record is ignored"
            ));
            continue;
        }
        let Some(entries) = versions.as_array() else {
            warnings.push(format!(
                "{filename}: {key} must contain a list of exact DSH versions; the record is ignored"
            ));
            continue;
        };
        // A non-string entry is refused rather than skipped: a list that silently drops what it
        // cannot read would drop the version the user meant to authorize.
        let mut versions_of_key = Vec::with_capacity(entries.len());
        for entry in entries {
            if let Some(version) = entry.as_str() {
                versions_of_key.push(version.to_owned());
            } else {
                versions_of_key.clear();
                break;
            }
        }
        if versions_of_key.len() != entries.len()
            || !versions_of_key
                .iter()
                .all(|version| is_exact_plugin_version(version))
        {
            warnings.push(format!(
                "{filename}: {key} must contain a list of exact DSH versions; the record is ignored"
            ));
            continue;
        }
        exemptions.insert(key.clone(), versions_of_key);
    }
    let rewritable = warnings.is_empty();
    CompatibilityFile {
        exemptions,
        warnings,
        rewritable,
    }
}

/// A refusal from the exemption rules — the error side of `validatePluginVersionExemption` and
/// `setProfileVersionExemption`.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum ExemptionError {
    /// Either identity is not a canonical exact version or package name.
    #[error(
        "Version exemptions require an exact npm package-name@version and an exact DSH runtime version"
    )]
    InvalidIdentity,
    /// A grant without an acknowledged risk.
    #[error(
        "Incompatible plugins may cause crashes or data loss. To grant this exact-version exemption, explicitly acknowledge the risk with --accept-risk (acceptRisk: true)."
    )]
    RiskNotAcknowledged,
    /// A grant naming a runtime this application does not run.
    #[error(
        "Cannot approve DSH {requested}: this application runs DSH {current}. Use --dsh-version {current}."
    )]
    StaleRuntime {
        /// The version the grant named.
        requested: String,
        /// The version this application runs.
        current: String,
    },
    /// The file holds content this reader rejected, so rewriting it would discard it.
    #[error("{filename} must be repaired before exemptions change:\n{}", warnings.join("\n"))]
    RepairRequired {
        /// The file's display name.
        filename: String,
        /// What the reader rejected.
        warnings: Vec<String>,
    },
}

/// Validate one explicit exemption without granting it — upstream
/// `validatePluginVersionExemption`.
///
/// The `package_version` is an exact `package-name@version`, never an installation spec or a range.
///
/// # Errors
///
/// Returns [`ExemptionError::InvalidIdentity`] when either identity is not canonical.
pub fn validate_plugin_version_exemption(
    package_version: &str,
    runtime_version: &str,
) -> Result<(), ExemptionError> {
    let valid = split_package_version(package_version).is_some_and(|(name, version)| {
        is_exemption_package_name(name) && is_exact_plugin_version(version)
    }) && is_exact_plugin_version(runtime_version);
    if valid {
        Ok(())
    } else {
        Err(ExemptionError::InvalidIdentity)
    }
}

/// Apply one informed grant or revocation to a compatibility file's contents — the decision half
/// of upstream `setProfileVersionExemption`.
///
/// The write itself is the caller's: it must happen under the profile's own file lock and be
/// atomic, and those are properties of the profile directory, not of this function. What is decided
/// here is the whole rule set — canonical identities, required consent, a grant only for the running
/// runtime, and a file that must be repaired by hand before anything is rewritten.
///
/// # Errors
///
/// Returns [`ExemptionError`] for an invalid identity, a grant without acknowledged risk, a grant
/// naming another runtime, or a file this reader rejected.
pub fn update_profile_compatibility(
    file: &CompatibilityFile,
    package_version: &str,
    runtime_version: &str,
    enabled: bool,
    accept_risk: bool,
    current_runtime: &str,
) -> Result<BTreeMap<String, Vec<String>>, ExemptionError> {
    validate_plugin_version_exemption(package_version, runtime_version)?;
    if enabled && !accept_risk {
        return Err(ExemptionError::RiskNotAcknowledged);
    }
    if enabled && runtime_version != current_runtime {
        return Err(ExemptionError::StaleRuntime {
            requested: runtime_version.to_owned(),
            current: current_runtime.to_owned(),
        });
    }
    if !file.rewritable {
        return Err(ExemptionError::RepairRequired {
            filename: PROFILE_COMPATIBILITY_FILENAME.to_owned(),
            warnings: file.warnings.clone(),
        });
    }
    let mut exemptions = file.exemptions.clone();
    let versions = exemptions.get(package_version).cloned().unwrap_or_default();
    if enabled {
        let mut granted = versions;
        if !granted.iter().any(|version| version == runtime_version) {
            granted.push(runtime_version.to_owned());
        }
        exemptions.insert(package_version.to_owned(), granted);
    } else {
        let retained: Vec<String> = versions
            .into_iter()
            .filter(|version| version != runtime_version)
            .collect();
        if retained.is_empty() {
            exemptions.remove(package_version);
        } else {
            exemptions.insert(package_version.to_owned(), retained);
        }
    }
    Ok(exemptions)
}

/// Render the document a grant writes: two-space indentation and a trailing newline, with keys in
/// sorted order so two identical grants produce the same bytes.
#[must_use]
pub fn render_compatibility(exemptions: &BTreeMap<String, Vec<String>>) -> String {
    let mut out = String::from("{\n");
    for (index, (key, versions)) in exemptions.iter().enumerate() {
        let _ = write!(out, "  {}", json_string(key));
        out.push_str(": [");
        for (position, version) in versions.iter().enumerate() {
            if position > 0 {
                out.push_str(", ");
            }
            out.push_str(&json_string(version));
        }
        out.push(']');
        if index + 1 < exemptions.len() {
            out.push(',');
        }
        out.push('\n');
    }
    out.push_str("}\n");
    out
}

/// One JSON string literal, quoted and escaped as JSON requires.
fn json_string(text: &str) -> String {
    let mut out = String::with_capacity(text.len() + 2);
    out.push('"');
    for character in text.chars() {
        match character {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            character if (character as u32) < 0x20 => {
                let _ = write!(out, "\\u{:04x}", character as u32);
            }
            character => out.push(character),
        }
    }
    out.push('"');
    out
}

/// One published version newer than the installed one, before its own manifest is read —
/// upstream `PublishedVersion`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PublishedVersion {
    /// The published version.
    pub version: String,
    /// The registry's publication time, as an ISO 8601 string.
    pub published_at: Option<String>,
    /// The dist-tags pointing at this version, such as `latest`.
    pub tags: Vec<String>,
    /// Whether the version is a prerelease.
    pub prerelease: bool,
}

/// A `pnpm view` answer that names no versions or no orderable installed version.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum NewerVersionsError {
    /// The answer names no version list.
    #[error("pnpm view answered no versions")]
    NoVersions,
    /// The installed version is not one semver can order.
    #[error("the installed version is not a semantic version: {installed}")]
    InvalidInstalled {
        /// The version as given.
        installed: String,
    },
}

/// The published versions newer than the installed one, newest first — upstream `newerVersions`.
///
/// `answer` is parsed `pnpm view <name> versions time dist-tags --json` output. `pnpm` prints a
/// package with one version as a string rather than a one-element array, and a version that is not
/// a semantic version is dropped rather than guessed at.
///
/// # Errors
///
/// Returns [`NewerVersionsError::NoVersions`] when the answer names no version list at all, and
/// [`NewerVersionsError::InvalidInstalled`] when the installed version is not one semver can order:
/// an update list relative to an unorderable version is a list of everything, which would offer to
/// "upgrade" a package to an older release.
pub fn newer_versions(
    answer: &Value,
    installed: &str,
) -> Result<Vec<PublishedVersion>, NewerVersionsError> {
    let installed =
        SemVer::parse(installed).ok_or_else(|| NewerVersionsError::InvalidInstalled {
            installed: installed.to_owned(),
        })?;
    let fields = answer.as_object().ok_or(NewerVersionsError::NoVersions)?;
    let listed: Vec<&str> = match fields.get("versions") {
        Some(Value::String(version)) => vec![version.as_str()],
        Some(Value::Array(versions)) => versions.iter().filter_map(Value::as_str).collect(),
        _ => return Err(NewerVersionsError::NoVersions),
    };
    let times = fields.get("time").and_then(Value::as_object);
    let tags: Vec<(String, String)> = fields
        .get("dist-tags")
        .and_then(Value::as_object)
        .map(|tags| {
            tags.iter()
                .filter_map(|(tag, version)| {
                    version
                        .as_str()
                        .map(|version| (tag.clone(), version.to_owned()))
                })
                .collect()
        })
        .unwrap_or_default();
    let mut newer: Vec<(SemVer, String)> = listed
        .into_iter()
        .filter_map(|version| SemVer::parse(version).map(|parsed| (parsed, version.to_owned())))
        .filter(|(parsed, _)| *parsed > installed)
        .collect();
    newer.sort_by(|(left, _), (right, _)| right.cmp(left));
    Ok(newer
        .into_iter()
        .map(|(parsed, version)| PublishedVersion {
            published_at: times
                .and_then(|times| times.get(&version))
                .and_then(Value::as_str)
                .map(str::to_owned),
            tags: tags
                .iter()
                .filter(|(_, tagged)| *tagged == version)
                .map(|(tag, _)| tag.clone())
                .collect(),
            prerelease: parsed.is_prerelease(),
            version,
        })
        .collect())
}

// ============================================================================================
// Host admission — upstream `app-boot/src/bundle-platform.ts`, `util/native-command`'s `isWsl`
// ============================================================================================

/// The Node platform names a bundle may declare (`NODE_PLATFORMS`).
pub const NODE_PLATFORMS: [&str; 11] = [
    "aix", "android", "darwin", "freebsd", "haiku", "linux", "openbsd", "sunos", "win32", "cygwin",
    "netbsd",
];

/// The environment and kernel facts that admit `wsl` on a Linux host — the `HostEnvironmentFacts`
/// of upstream `util/native-command`.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct HostFacts {
    /// The host's platform, as Node names it (`process.platform`).
    pub platform: String,
    /// `WSL_DISTRO_NAME`, when the environment has it.
    pub wsl_distro_name: Option<String>,
    /// `WSL_INTEROP`, when the environment has it.
    pub wsl_interop: Option<String>,
    /// The kernel release (`os.release()`), when it could be read.
    pub os_release: Option<String>,
}

/// Whether the host is Windows Subsystem for Linux — upstream `isWsl`.
///
/// Only a Linux host can be WSL, and the evidence is either WSL's own environment variables or a
/// kernel release naming Microsoft. Both are checked because a login shell inside a distribution
/// container may have neither variable while the kernel still says `microsoft`.
#[must_use]
pub fn is_wsl(facts: &HostFacts) -> bool {
    if !facts.platform.eq_ignore_ascii_case("linux") {
        return false;
    }
    let present = |value: &Option<String>| value.as_deref().is_some_and(|value| !value.is_empty());
    present(&facts.wsl_distro_name)
        || present(&facts.wsl_interop)
        || facts
            .os_release
            .as_deref()
            .is_some_and(|release| release.to_ascii_lowercase().contains("microsoft"))
}

/// A bundle whose declared platforms exclude the host — upstream `UnsupportedBundlePlatformError`.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
#[error("Bundle supports Host platforms {platforms}; the DSH Host is running {platform}")]
pub struct UnsupportedPlatform {
    /// The platforms the bundle declared, comma-separated in declaration order.
    pub platforms: String,
    /// The host's platform.
    pub platform: String,
}

/// A `dsh.bundle.platforms` declaration that is not a list of Node platform names or `wsl`.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
#[error("dsh.bundle.platforms must be a nonempty list of Node platform names or wsl")]
pub struct MalformedPlatforms;

/// Why a bundle may not be selected on this host.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum PlatformError {
    /// The declaration itself is unusable.
    #[error(transparent)]
    Malformed(#[from] MalformedPlatforms),
    /// The bundle declares platforms and this host is not one of them.
    #[error(transparent)]
    Unsupported(#[from] UnsupportedPlatform),
}

/// Whether a `dsh.bundle.platforms` value is a non-empty list of known names, and which names it
/// holds.
///
/// The value arrives as decoded JSON, so the shape is checked here rather than assumed: a bundle
/// that wrote `"platforms": "linux"` instead of `["linux"]` would otherwise be admitted on every
/// host, which is the one outcome the declaration exists to prevent.
///
/// # Errors
///
/// Returns [`MalformedPlatforms`] for an empty list, a non-string entry, or an unknown name.
pub fn declared_platforms(value: &Value) -> Result<Vec<String>, MalformedPlatforms> {
    let Some(entries) = value.as_array() else {
        return Err(MalformedPlatforms);
    };
    if entries.is_empty() {
        return Err(MalformedPlatforms);
    }
    let mut platforms = Vec::with_capacity(entries.len());
    for entry in entries {
        let Some(name) = entry.as_str() else {
            return Err(MalformedPlatforms);
        };
        if name != "wsl" && !NODE_PLATFORMS.contains(&name) {
            return Err(MalformedPlatforms);
        }
        platforms.push(name.to_owned());
    }
    Ok(platforms)
}

/// Validate a bundle's platform declaration before selecting or composing it — upstream
/// `assertBundlePlatform`.
///
/// Omission admits every platform; a malformed declaration is refused rather than ignored, because
/// ignoring it admits the bundle on the very host it was meant to exclude.
///
/// # Errors
///
/// Returns [`PlatformError::Malformed`] for an unusable declaration and
/// [`PlatformError::Unsupported`] when the host is not among the declared platforms.
pub fn assert_bundle_platform(
    platforms: Option<&Value>,
    facts: &HostFacts,
) -> Result<(), PlatformError> {
    let Some(value) = platforms else {
        return Ok(());
    };
    let declared = declared_platforms(value)?;
    if declared.contains(&facts.platform) {
        return Ok(());
    }
    if declared.iter().any(|name| name == "wsl") && is_wsl(facts) {
        return Ok(());
    }
    Err(PlatformError::Unsupported(UnsupportedPlatform {
        platforms: declared.join(", "),
        platform: facts.platform.clone(),
    }))
}

#[cfg(test)]
mod tests {
    use super::{is_absolute_path, is_exact_plugin_version, is_registry_package_name};

    #[test]
    fn absolute_paths_are_recognised() {
        assert!(is_absolute_path("/plugins/x"));
        assert!(is_absolute_path("C:\\plugins\\x"));
        assert!(!is_absolute_path("./x"));
        assert!(!is_absolute_path("plugins/x"));
    }

    #[test]
    fn package_names_follow_the_registry_rule() {
        assert!(is_registry_package_name("dsh-x"));
        assert!(is_registry_package_name("@acme/dsh-x"));
        assert!(!is_registry_package_name("Dsh-Upper"));
        assert!(!is_registry_package_name(".hidden"));
        assert!(!is_registry_package_name("@acme"));
    }

    #[test]
    fn exact_versions_refuse_a_prefix_or_a_range() {
        assert!(is_exact_plugin_version("1.2.3"));
        assert!(is_exact_plugin_version("1.2.3-alpha.1+build.5"));
        assert!(!is_exact_plugin_version("v1.2.3"));
        assert!(!is_exact_plugin_version("^1.2.3"));
        assert!(!is_exact_plugin_version("1.2"));
    }
}
