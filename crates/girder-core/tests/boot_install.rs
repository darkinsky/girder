//! Tests for `boot_gap`: the plugin admission and package-logistics decisions of upstream
//! `packages/boot`.
//!
//! The cases are the upstream ones, restated where the upstream suites state them
//! (`plugin-manager/tests/install-spec.spec.ts`, `plugin-manager/tests/registry.spec.ts`,
//! `app-boot/tests/plugin-compatibility.spec.ts`, `app-boot/tests/profile-compatibility.spec.ts`),
//! plus the ones whose *reason* is the property under test rather than a recorded output: a private
//! registry never falling through to a public one, a malformed platform declaration never admitting
//! a bundle, and an unrepairable compatibility file never being rewritten.
//!
//! # Why the module is compiled by path
//!
//! The crate root's wiring is a separate change, so this compiles the exact source file rather than
//! a copy. Inside a `#[path]` module the `crate::` prefix would name *this* test crate, not
//! `girder-core`, so every reference to the library goes through `girder_core::{...}` — which is
//! why the module itself imports nothing from the crate root.

#[path = "../src/boot/install.rs"]
pub mod boot_gap;

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use serde_json::json;

use boot_gap::{
    CompatibilityFile, ExemptionError, FailedAt, HostFacts, InstallFailureFacts,
    InstallFailureKind, MalformedPlatforms, NPMMIRROR_REGISTRY, NewerVersionsError,
    OFFICIAL_NPM_REGISTRY, ParsedSpec, PlatformError, Registries, RegistryRequest, SemVer,
    SpecKind, assert_bundle_platform, attribute_failure, classify_install_failure,
    declared_platforms, dependency_spec, is_exact_plugin_version, is_registry_url, is_wsl,
    newer_versions, normalize_registry, parse_compatibility, parse_install_spec, pinned_git_origin,
    read_profile_compatibility, registry_plan, render_compatibility, update_profile_compatibility,
    validate_plugin_version_exemption,
};

/// A scratch directory that the test owns.
fn scratch(name: &str) -> PathBuf {
    static COUNTER: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
    let n = COUNTER.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let dir =
        std::env::temp_dir().join(format!("girder-boot-gap-{name}-{}-{n}", std::process::id()));
    std::fs::create_dir_all(&dir).expect("created the scratch directory");
    dir
}

/// A parsed spec, or a panic naming the refusal (a refusal here is a failed test, not a case).
fn parsed(spec: &str) -> ParsedSpec {
    match parse_install_spec(spec) {
        Ok(parsed) => parsed,
        Err(error) => panic!("{spec} was refused: {error}"),
    }
}

/// The refusal reason for a spec that must be refused.
fn refusal(spec: &str) -> String {
    match parse_install_spec(spec) {
        Ok(parsed) => panic!("{spec} was accepted as {}", parsed.kind().label()),
        Err(error) => {
            // Upstream asserts the message shape as well as the reason: the reason reaches a person,
            // and the spec it names must be the trimmed one.
            assert_eq!(error.spec, spec.trim());
            assert_eq!(
                error.to_string(),
                format!("plugin-manager: {}: {}", error.reason, spec.trim())
            );
            error.reason
        }
    }
}

// ---- install specs -------------------------------------------------------------------------

#[test]
fn registry_names_carry_an_optional_range() {
    assert_eq!(
        parsed(" dsh-better-sidebar "),
        ParsedSpec::Registry {
            spec: "dsh-better-sidebar".to_owned(),
            name: "dsh-better-sidebar".to_owned(),
            range: None,
        }
    );
    assert_eq!(
        parsed("@acme/dsh-tool@^1.2"),
        ParsedSpec::Registry {
            spec: "@acme/dsh-tool@^1.2".to_owned(),
            name: "@acme/dsh-tool".to_owned(),
            range: Some("^1.2".to_owned()),
        }
    );
    assert_eq!(
        parsed("pkg@latest"),
        ParsedSpec::Registry {
            spec: "pkg@latest".to_owned(),
            name: "pkg".to_owned(),
            range: Some("latest".to_owned()),
        }
    );
}

#[test]
fn absolute_paths_and_local_tarballs_are_read_as_paths() {
    for (spec, path) in [
        ("/plugins/dsh-x", "/plugins/dsh-x"),
        ("file:/plugins/dsh-x", "/plugins/dsh-x"),
        ("link:/plugins/dsh-x", "/plugins/dsh-x"),
    ] {
        assert_eq!(
            parsed(spec),
            ParsedSpec::Path {
                spec: spec.to_owned(),
                path: path.to_owned(),
            },
            "{spec}"
        );
    }
    assert_eq!(
        parsed("/packs/dsh-x-1.0.0.tgz"),
        ParsedSpec::Tarball {
            spec: "/packs/dsh-x-1.0.0.tgz".to_owned(),
            path: Some("/packs/dsh-x-1.0.0.tgz".to_owned()),
            host: None,
        }
    );
    // A Windows drive path is absolute too; resolving it against the launch directory would name a
    // directory inside the profile instead of the one the user typed.
    assert_eq!(parsed("C:\\plugins\\dsh-x").kind(), SpecKind::Path);
}

#[test]
fn git_hosts_and_tarball_urls_name_the_host_they_are_fetched_from() {
    for (spec, host) in [
        ("github:someone/dsh-plugin", "github.com"),
        ("gitlab:a/b#main", "gitlab.com"),
        ("bitbucket:a/b", "bitbucket.org"),
        ("gist:0123abcd", "gist.github.com"),
        ("git+ssh://git@github.com/a/b.git", "github.com"),
        ("git://host/a/b", "host"),
        ("git@github.com:a/b.git", "github.com"),
        ("https://github.com/a/b", "github.com"),
        ("https://github.com/a/b.git#v1", "github.com"),
        (
            "git+https://Git.Example.com:8443/a/b.git",
            "git.example.com:8443",
        ),
    ] {
        let parsed = parsed(spec);
        assert_eq!(parsed.kind(), SpecKind::Git, "{spec}");
        assert_eq!(parsed.host(), Some(host), "{spec}");
    }
    assert_eq!(
        parsed("https://cdn.example.com/x/y/z/dsh-x-1.0.0.tgz"),
        ParsedSpec::Tarball {
            spec: "https://cdn.example.com/x/y/z/dsh-x-1.0.0.tgz".to_owned(),
            path: None,
            host: Some("cdn.example.com".to_owned()),
        }
    );
}

#[test]
fn refusals_name_the_rule_that_was_broken() {
    assert_eq!(refusal("   "), "the package spec must not be empty");
    assert_eq!(refusal("./dsh-x"), "a local path must be absolute");
    assert_eq!(refusal("../dsh-x"), "a local path must be absolute");
    assert_eq!(refusal("file:./dsh-x"), "a local path must be absolute");
    assert_eq!(
        refusal("https://example.com/not-a-package"),
        "a URL must point at a git repository or a tarball"
    );
    assert_eq!(
        refusal("Dsh-Upper"),
        "not a package name the registry accepts"
    );
    assert_eq!(
        refusal(".hidden"),
        "not a package name the registry accepts"
    );
    assert_eq!(
        refusal("has space"),
        "not a package name the registry accepts"
    );
    assert_eq!(
        refusal(&"a".repeat(215)),
        "not a package name the registry accepts"
    );
    assert_eq!(refusal("pkg@"), "a version after @ must not be empty");
}

#[test]
fn a_pinned_git_origin_is_one_repository_across_transports() {
    const COMMIT: &str = "0123456789abcdef0123456789abcdef01234567";
    for spec in [
        format!("git+https://GitLab.Example.com/g/plugins.git#{COMMIT}&path:/packages/a"),
        format!("git+ssh://git@gitlab.example.com/g/plugins.git#path:/packages/a/&{COMMIT_UPPER}"),
        format!("git@gitlab.example.com:g/plugins.git/#{COMMIT}&path:packages/a"),
    ] {
        assert_eq!(
            pinned_git_origin(&spec).as_deref(),
            Some("gitlab.example.com/g/plugins#packages/a"),
            "{spec}"
        );
    }
    // `pnpm` saves a known host's shortcut for the URL it was given, so the two forms are one origin.
    assert_eq!(
        pinned_git_origin(&format!("github:o/r#{COMMIT}&path:/p")),
        pinned_git_origin(&format!("git+https://github.com/o/r.git#{COMMIT}&path:/p"))
    );
    assert_eq!(
        pinned_git_origin(&format!("https://github.com/a/b#{COMMIT}")).as_deref(),
        Some("github.com/a/b#")
    );
    // A port is part of the host, not an scp-like path separator.
    assert_eq!(
        pinned_git_origin(&format!("git+ssh://git@host:2222/a/b.git#{COMMIT}")).as_deref(),
        Some("host:2222/a/b#")
    );
    assert_eq!(
        pinned_git_origin(&format!(
            "git+ssh://git@host:2222/a/b.git#{}",
            COMMIT.to_uppercase()
        ))
        .as_deref(),
        Some("host:2222/a/b#")
    );
}

/// The commit above, upper-cased, for the fragment-order case.
const COMMIT_UPPER: &str = "0123456789ABCDEF0123456789ABCDEF01234567";

#[test]
fn a_git_spec_without_one_commit_and_one_directory_has_no_origin() {
    const COMMIT: &str = "0123456789abcdef0123456789abcdef01234567";
    let short = &COMMIT[1..];
    for spec in [
        String::new(),
        "dsh-x@1.0.0".to_owned(),
        "/plugins/dsh-x".to_owned(),
        "git+https://host/a/b.git".to_owned(),
        "git+https://host/a/b.git#main&path:/a".to_owned(),
        format!("git+https://host/a/b.git#{short}"),
        format!("git+https://host/a/b.git#{COMMIT}&path:/a&path:/b"),
        format!("git+https://host/a/b.git#{COMMIT}&{COMMIT}"),
    ] {
        assert_eq!(pinned_git_origin(&spec), None, "{spec}");
    }
}

#[test]
fn a_recorded_dependency_becomes_the_spec_pnpm_add_accepts() {
    let profile = Path::new("/home/user/.dsh/profiles/dev");
    let home = Path::new("/home/user");
    let same = |recorded: &str| dependency_spec("pkg", recorded, profile, home, "pkg");

    // A registry range or tag follows the name.
    assert_eq!(same("^1.2.3"), "pkg@^1.2.3");
    assert_eq!(same("latest"), "pkg@latest");
    // A relative local path becomes absolute against the profile directory.
    assert_eq!(same("file:../local"), "file:/home/user/.dsh/profiles/local");
    // `~` is the home directory, and a `link:` keeps its prefix.
    assert_eq!(same("link:~/plugins/a"), "link:/home/user/plugins/a");
    // An HTTP(S) URL loses its credentials but keeps its query string.
    assert_eq!(
        same("https://user:token@github.com/a/b.git?ref=main"),
        "https://github.com/a/b.git?ref=main"
    );
    // Any other protocol is copied as written.
    assert_eq!(same("git+ssh://git@host/a/b"), "git+ssh://git@host/a/b");
    // An alias records the spec under the name it was installed as.
    assert_eq!(
        dependency_spec("alias", "^1.0.0", profile, home, "real"),
        "alias@^1.0.0"
    );
    assert_eq!(
        dependency_spec("alias", "https://host/a/b.tgz", profile, home, "real"),
        "alias@https://host/a/b.tgz"
    );
}

// ---- registries ----------------------------------------------------------------------------

#[test]
fn a_registry_is_normalized_the_way_pnpm_compares_one() {
    assert_eq!(
        normalize_registry("https://REGISTRY.npmmirror.com").as_deref(),
        Ok(NPMMIRROR_REGISTRY)
    );
    assert_eq!(
        normalize_registry("http://npm.corp.example:4873/prefix").as_deref(),
        Ok("http://npm.corp.example:4873/prefix/")
    );
    assert_eq!(
        normalize_registry(NPMMIRROR_REGISTRY).as_deref(),
        Ok(NPMMIRROR_REGISTRY)
    );
    for url in [
        "registry.npmmirror.com",
        "ftp://x/",
        "file:///tmp",
        "",
        "https://",
    ] {
        assert!(normalize_registry(url).is_err(), "{url}");
        assert!(!is_registry_url(url), "{url}");
    }
    assert!(is_registry_url(NPMMIRROR_REGISTRY));
}

/// The shipped configuration on a machine whose `pnpm` names npm's own registry.
fn shipped() -> Registries {
    Registries {
        registry: None,
        fallback_registries: vec![NPMMIRROR_REGISTRY.to_owned()],
        resolved: Some(OFFICIAL_NPM_REGISTRY.to_owned()),
    }
}

/// A plan, or a panic naming the refusal.
fn plan(requested: &RegistryRequest, configured: &Registries) -> Vec<Option<String>> {
    match registry_plan(requested, configured) {
        Ok(plan) => plan,
        Err(error) => panic!("the plan was refused: {error}"),
    }
}

#[test]
fn the_plan_asks_pnpm_own_registry_then_the_fallbacks() {
    assert_eq!(
        plan(&RegistryRequest::Configured, &shipped()),
        vec![None, Some(NPMMIRROR_REGISTRY.to_owned())]
    );
    assert_eq!(
        plan(&RegistryRequest::PnpmOwn, &shipped()),
        vec![None, Some(NPMMIRROR_REGISTRY.to_owned())]
    );
}

#[test]
fn a_requested_registry_moves_to_the_front_of_the_configured_set() {
    assert_eq!(
        plan(
            &RegistryRequest::Url(NPMMIRROR_REGISTRY.to_owned()),
            &shipped()
        ),
        vec![Some(NPMMIRROR_REGISTRY.to_owned()), None]
    );
    assert_eq!(
        plan(
            &RegistryRequest::Url("https://REGISTRY.npmmirror.com".to_owned()),
            &shipped()
        ),
        vec![Some(NPMMIRROR_REGISTRY.to_owned()), None]
    );
    assert_eq!(
        plan(
            &RegistryRequest::PnpmOwn,
            &Registries {
                registry: Some(NPMMIRROR_REGISTRY.to_owned()),
                fallback_registries: Vec::new(),
                resolved: Some(OFFICIAL_NPM_REGISTRY.to_owned()),
            }
        ),
        vec![None]
    );
}

#[test]
fn a_registry_outside_the_configured_set_is_asked_alone() {
    const CORP: &str = "https://npm.corp.example/";
    assert_eq!(
        plan(&RegistryRequest::Url(CORP.to_owned()), &shipped()),
        vec![Some(CORP.to_owned())]
    );
    assert_eq!(
        plan(
            &RegistryRequest::Configured,
            &Registries {
                registry: Some(CORP.to_owned()),
                fallback_registries: Vec::new(),
                resolved: Some(OFFICIAL_NPM_REGISTRY.to_owned()),
            }
        ),
        vec![Some(CORP.to_owned())]
    );
    assert_eq!(
        plan(
            &RegistryRequest::Configured,
            &Registries {
                registry: Some(CORP.to_owned()),
                fallback_registries: vec![NPMMIRROR_REGISTRY.to_owned()],
                resolved: Some(OFFICIAL_NPM_REGISTRY.to_owned()),
            }
        ),
        vec![Some(CORP.to_owned()), Some(NPMMIRROR_REGISTRY.to_owned())]
    );
}

#[test]
fn a_private_registry_of_pnpms_own_never_falls_through_to_a_public_one() {
    const CORP: &str = "https://npm.corp.example/";
    let corporate = Registries {
        resolved: Some(CORP.to_owned()),
        ..shipped()
    };
    assert_eq!(plan(&RegistryRequest::Configured, &corporate), vec![None]);
    assert_eq!(plan(&RegistryRequest::PnpmOwn, &corporate), vec![None]);
    // The same registry typed by URL is the private one too.
    assert_eq!(
        plan(&RegistryRequest::Url(CORP.to_owned()), &corporate),
        vec![Some(CORP.to_owned())]
    );
    // A public mirror picked over a private default is asked alone: the private registry is not a
    // fallback for it.
    assert_eq!(
        plan(
            &RegistryRequest::Url(NPMMIRROR_REGISTRY.to_owned()),
            &corporate
        ),
        vec![Some(NPMMIRROR_REGISTRY.to_owned())]
    );
}

#[test]
fn an_unknown_pnpm_registry_is_asked_alone() {
    let unknown = Registries {
        resolved: None,
        ..shipped()
    };
    assert_eq!(plan(&RegistryRequest::Configured, &unknown), vec![None]);
    assert_eq!(
        plan(
            &RegistryRequest::Url(NPMMIRROR_REGISTRY.to_owned()),
            &unknown
        ),
        vec![Some(NPMMIRROR_REGISTRY.to_owned())]
    );
}

#[test]
fn a_registry_pnpms_own_configuration_already_names_is_asked_once() {
    const CORP: &str = "https://npm.corp.example/";
    let mirrored = Registries {
        resolved: Some(NPMMIRROR_REGISTRY.to_owned()),
        ..shipped()
    };
    assert_eq!(plan(&RegistryRequest::Configured, &mirrored), vec![None]);
    assert_eq!(
        plan(
            &RegistryRequest::Url(NPMMIRROR_REGISTRY.to_owned()),
            &mirrored
        ),
        vec![Some(NPMMIRROR_REGISTRY.to_owned())]
    );
    assert_eq!(
        plan(
            &RegistryRequest::Configured,
            &Registries {
                registry: None,
                fallback_registries: vec![NPMMIRROR_REGISTRY.to_owned(), CORP.to_owned()],
                resolved: Some(NPMMIRROR_REGISTRY.to_owned()),
            }
        ),
        vec![None, Some(CORP.to_owned())]
    );
}

#[test]
fn each_registry_is_listed_once() {
    const CORP: &str = "https://npm.corp.example/";
    assert_eq!(
        plan(
            &RegistryRequest::Configured,
            &Registries {
                registry: Some(NPMMIRROR_REGISTRY.to_owned()),
                fallback_registries: vec![NPMMIRROR_REGISTRY.to_owned(), CORP.to_owned()],
                resolved: Some(OFFICIAL_NPM_REGISTRY.to_owned()),
            }
        ),
        vec![Some(NPMMIRROR_REGISTRY.to_owned()), Some(CORP.to_owned())]
    );
}

// ---- failure attribution -------------------------------------------------------------------

#[test]
fn a_failure_another_registry_could_change_is_laid_at_the_registry() {
    let name = parsed("dsh-x");
    let registry_log = "ERR_PNPM_META_FETCH_FAIL  GET https://registry.npmjs.org/dsh-x: getaddrinfo ENOTFOUND registry.npmjs.org";
    for kind in [
        InstallFailureKind::Network,
        InstallFailureKind::Timeout,
        InstallFailureKind::NotFound,
        InstallFailureKind::NoMatchingVersion,
    ] {
        assert_eq!(
            attribute_failure(kind, registry_log, &name),
            FailedAt::Registry,
            "{kind:?}"
        );
    }
}

#[test]
fn a_failure_no_registry_would_change_is_laid_at_neither() {
    let name = parsed("dsh-x");
    let registry_log = "ERR_PNPM_META_FETCH_FAIL  GET https://registry.npmjs.org/dsh-x: getaddrinfo ENOTFOUND registry.npmjs.org";
    for kind in [
        InstallFailureKind::BuildBlocked,
        InstallFailureKind::DiskFull,
        InstallFailureKind::Permission,
        InstallFailureKind::Integrity,
        InstallFailureKind::PnpmMissing,
        InstallFailureKind::Unknown,
    ] {
        assert_eq!(
            attribute_failure(kind, registry_log, &name),
            FailedAt::Other,
            "{kind:?}"
        );
    }
}

#[test]
fn a_failure_naming_the_specs_own_host_is_laid_at_that_host() {
    let git = parsed("github:acme/dsh-x");
    let tarball = parsed("https://cdn.example.com/dsh-x-1.0.0.tgz");
    let registry_log = "ERR_PNPM_META_FETCH_FAIL  GET https://registry.npmjs.org/dsh-x: getaddrinfo ENOTFOUND registry.npmjs.org";
    assert_eq!(
        attribute_failure(
            InstallFailureKind::Network,
            "fatal: unable to access 'https://github.com/acme/dsh-x/': Could not resolve host: github.com",
            &git
        ),
        FailedAt::SpecHost
    );
    assert_eq!(
        attribute_failure(
            InstallFailureKind::Network,
            "ssh: Could not resolve hostname GITHUB.COM: nodename nor servname provided",
            &git
        ),
        FailedAt::SpecHost
    );
    assert_eq!(
        attribute_failure(
            InstallFailureKind::Network,
            "ERR_PNPM_FETCH_502  GET https://cdn.example.com/dsh-x-1.0.0.tgz: Bad Gateway",
            &tarball
        ),
        FailedAt::SpecHost
    );
    // The same specs' dependencies still come from the registry.
    assert_eq!(
        attribute_failure(InstallFailureKind::Network, registry_log, &git),
        FailedAt::Registry
    );
    assert_eq!(
        attribute_failure(InstallFailureKind::Network, registry_log, &tarball),
        FailedAt::Registry
    );
    assert_eq!(
        attribute_failure(
            InstallFailureKind::NotFound,
            "ERR_PNPM_FETCH_404  GET https://registry.npmjs.org/left-pad: Not Found - 404",
            &git
        ),
        FailedAt::Registry
    );
}

#[test]
fn the_spec_host_is_read_off_error_lines_and_not_off_a_warning_that_links_to_it() {
    let git = parsed("github:acme/dsh-x");
    let log = [
        "WARN  deprecated left-pad@1.0.0: see https://github.com/acme/dsh-x#readme",
        "ERR_PNPM_META_FETCH_FAIL  GET https://registry.npmjs.org/left-pad: ETIMEDOUT",
    ]
    .join("\n");
    assert_eq!(
        attribute_failure(InstallFailureKind::Network, &log, &git),
        FailedAt::Registry
    );
    assert_eq!(
        attribute_failure(InstallFailureKind::Network, "ECONNRESET", &git),
        FailedAt::Registry
    );
    let name = parsed("dsh-x");
    assert_eq!(
        attribute_failure(InstallFailureKind::Network, "socket hang up", &name),
        FailedAt::Registry
    );
}

// ---- failure classification ----------------------------------------------------------------

/// The kind for a log, with no spawn error and no timeout.
fn kind(log: &str) -> InstallFailureKind {
    classify_install_failure(&InstallFailureFacts {
        log,
        cause_code: None,
        timed_out: false,
    })
}

#[test]
fn git_http_responses_are_not_read_as_connection_recovery() {
    for status in [401, 403, 404] {
        assert_eq!(
            kind(&format!(
                "fatal: unable to access 'https://github.com/acme/private.git/': The requested URL returned error: {status}"
            )),
            InstallFailureKind::Unknown,
            "{status}"
        );
    }
}

#[test]
fn the_end_of_the_run_is_read_before_its_output() {
    assert_eq!(
        classify_install_failure(&InstallFailureFacts {
            log: "ENOSPC",
            cause_code: None,
            timed_out: true,
        }),
        InstallFailureKind::Timeout
    );
    assert_eq!(
        classify_install_failure(&InstallFailureFacts {
            log: "",
            cause_code: Some("ENOENT"),
            timed_out: false,
        }),
        InstallFailureKind::PnpmMissing
    );
    assert_eq!(
        classify_install_failure(&InstallFailureFacts {
            log: "exited with 1",
            cause_code: Some("EACCES"),
            timed_out: false,
        }),
        InstallFailureKind::Unknown,
        "only ENOENT means the package manager is missing; another spawn error code is not read"
    );
}

#[test]
fn the_most_specific_code_in_the_output_names_the_failure() {
    for (log, expected) in [
        (
            "ERR_PNPM_IGNORED_BUILDS  Ignored build scripts: node-pty",
            InstallFailureKind::BuildBlocked,
        ),
        (
            "[ERR_PNPM_GIT_DEP_PREPARE_NOT_ALLOWED] Failed to prepare git-hosted package",
            InstallFailureKind::BuildBlocked,
        ),
        (
            "npm error code E404\nnpm error 404 Not Found - GET https://registry/x",
            InstallFailureKind::NotFound,
        ),
        (
            "ERR_PNPM_NO_MATCHING_VERSION  No matching version found for x@9",
            InstallFailureKind::NoMatchingVersion,
        ),
        (
            "npm error code ETARGET",
            InstallFailureKind::NoMatchingVersion,
        ),
        (
            "ENOSPC: no space left on device, write",
            InstallFailureKind::DiskFull,
        ),
        (
            "EACCES: permission denied, mkdir",
            InstallFailureKind::Permission,
        ),
        (
            "ERR_PNPM_TARBALL_INTEGRITY  Got sha512-...",
            InstallFailureKind::Integrity,
        ),
        (
            "ERR_PNPM_META_FETCH_FAIL  GET https://registry/x: request to https://registry/x failed, reason: getaddrinfo ENOTFOUND registry",
            InstallFailureKind::Network,
        ),
        ("ECONNRESET", InstallFailureKind::Network),
        (
            "ERR_PNPM_FETCH_502  GET https://registry/x: Bad Gateway",
            InstallFailureKind::Network,
        ),
        (
            "fatal: unable to access https://github.com/a/b/: Could not resolve host",
            InstallFailureKind::Network,
        ),
        // An offline cache miss is no registry's failure, so it is not one another registry could
        // change.
        (
            "ERR_PNPM_NO_OFFLINE_META  Failed to resolve x@1 in package mirror",
            InstallFailureKind::Unknown,
        ),
        (
            "npm error code FETCH_ERROR\nnpm error errno FETCH_ERROR",
            InstallFailureKind::Network,
        ),
        ("exited with 1", InstallFailureKind::Unknown),
        ("", InstallFailureKind::Unknown),
        // CJK text beside an errno name is a word boundary, so the errno is still read.
        ("安装失败：ECONNRESET", InstallFailureKind::Network),
    ] {
        assert_eq!(kind(log), expected, "{log}");
    }
}

// ---- versions ------------------------------------------------------------------------------

#[test]
fn semver_orders_by_precedence_and_ignores_build_metadata() {
    let ordered = [
        "1.0.0-alpha",
        "1.0.0-alpha.1",
        "1.0.0-alpha.beta",
        "1.0.0-beta",
        "1.0.0-beta.2",
        "1.0.0-beta.11",
        "1.0.0-rc.1",
        "1.0.0",
    ];
    for pair in ordered.windows(2) {
        let left = SemVer::parse(pair[0]).expect("parsed");
        let right = SemVer::parse(pair[1]).expect("parsed");
        assert!(left < right, "{} < {}", pair[0], pair[1]);
    }
    assert_eq!(SemVer::parse("1.0.0+build.5"), SemVer::parse("1.0.0+other"));
    assert!(
        SemVer::parse("1.0.0-alpha.1")
            .expect("parsed")
            .is_prerelease()
    );
    assert!(!SemVer::parse("1.0.0").expect("parsed").is_prerelease());
    // A leading `v` is tolerated by the parser but not by the exactness check, which is the
    // difference that makes an exemption refuse a spelling it cannot compare.
    assert_eq!(
        SemVer::parse("v1.2.3").map(|v| v.to_string()),
        Some("1.2.3".to_owned())
    );
    for malformed in ["1.2", "01.2.3", "1.2.3.4", "1.2.x", "", "1.2.3-", "1.2.3+"] {
        assert_eq!(SemVer::parse(malformed), None, "{malformed}");
    }
}

#[test]
fn an_exemption_names_an_exact_version_or_is_refused() {
    for exact in [
        "1.2.3",
        "1.2.3-alpha.1",
        "1.2.3+build.5",
        "1.2.3-alpha+build",
    ] {
        assert!(is_exact_plugin_version(exact), "{exact}");
    }
    for inexact in ["v1.2.3", "^1.2.3", "1.2", "*", " 1.2.3", "1.2.3 ", "latest"] {
        assert!(!is_exact_plugin_version(inexact), "{inexact}");
    }
}

#[test]
fn a_published_version_list_holds_only_what_is_newer() {
    let answer = json!({
        "versions": ["0.9.0", "1.0.0", "1.0.1", "1.1.0-rc.1", "1.1.0", "not-a-version"],
        "time": {
            "1.0.0": "2026-01-01T00:00:00.000Z",
            "1.0.1": "2026-02-01T00:00:00.000Z",
            "1.1.0": "2026-03-01T00:00:00.000Z"
        },
        "dist-tags": { "latest": "1.1.0", "next": "1.1.0-rc.1", "old": "0.9.0" }
    });
    let versions = newer_versions(&answer, "1.0.0").expect("read");
    assert_eq!(
        versions
            .iter()
            .map(|version| version.version.as_str())
            .collect::<Vec<_>>(),
        vec!["1.1.0", "1.1.0-rc.1", "1.0.1"]
    );
    assert_eq!(
        versions[0].published_at.as_deref(),
        Some("2026-03-01T00:00:00.000Z")
    );
    assert_eq!(versions[0].tags, vec!["latest".to_owned()]);
    assert_eq!(versions[1].tags, vec!["next".to_owned()]);
    assert!(versions[1].prerelease);
    assert!(!versions[0].prerelease);
    // The installed version itself is not newer than itself.
    assert!(
        newer_versions(&answer, "0.9.0")
            .expect("read")
            .iter()
            .all(|version| version.version != "0.9.0")
    );
}

#[test]
fn a_single_version_answer_is_a_string() {
    let answer = json!({ "versions": "2.0.0" });
    let versions = newer_versions(&answer, "1.0.0").expect("read");
    assert_eq!(versions.len(), 1);
    assert_eq!(versions[0].version, "2.0.0");
    assert_eq!(versions[0].published_at, None);
    assert!(versions[0].tags.is_empty());
}

#[test]
fn a_version_list_that_cannot_be_ordered_is_refused_rather_than_guessed() {
    assert_eq!(
        newer_versions(&json!({ "versions": ["1.0.0"] }), "not-a-version"),
        Err(NewerVersionsError::InvalidInstalled {
            installed: "not-a-version".to_owned()
        })
    );
    assert_eq!(
        newer_versions(&json!({}), "1.0.0"),
        Err(NewerVersionsError::NoVersions)
    );
    assert_eq!(
        newer_versions(&json!({ "versions": [] }), "1.0.0").map(|versions| versions.len()),
        Ok(0)
    );
}

// ---- compatibility records -----------------------------------------------------------------

const RUNTIME: &str = "0.1.7-alpha.1";
const FILENAME: &str = "compatibility.json";

#[test]
fn a_missing_compatibility_file_authorizes_nothing_and_is_rewritable() {
    let file = parse_compatibility(FILENAME, None);
    assert_eq!(
        file,
        CompatibilityFile {
            exemptions: BTreeMap::new(),
            warnings: Vec::new(),
            rewritable: true,
        }
    );
    // A fresh profile is not a broken one: reading a directory that has no file says nothing.
    let dir = scratch("compat-missing");
    let read = read_profile_compatibility(&dir);
    assert!(read.exemptions.is_empty());
    assert!(read.warnings.is_empty());
    assert!(read.rewritable);
}

#[test]
fn an_unreadable_compatibility_file_authorizes_nothing_and_must_not_be_rewritten() {
    for text in ["{", "[]", "1", "\"text\""] {
        let file = parse_compatibility(FILENAME, Some(text));
        assert!(file.exemptions.is_empty(), "{text}");
        assert_eq!(file.warnings.len(), 1, "{text}");
        assert!(!file.rewritable, "{text}");
    }
    let dir = scratch("compat-broken");
    std::fs::write(dir.join(FILENAME), "{").expect("wrote the file");
    let read = read_profile_compatibility(&dir);
    assert!(read.exemptions.is_empty());
    assert_eq!(read.warnings.len(), 1);
    assert!(!read.rewritable);
}

#[test]
fn each_rejected_record_is_reported_while_the_others_still_apply() {
    let text = format!(
        r#"{{
  "@example/plugin@1.0.0": ["{RUNTIME}"],
  "Not-A-Package@1.0.0": ["{RUNTIME}"],
  "@example/plugin@^1.0.0": ["{RUNTIME}"],
  "@example/plugin@2.0.0": ["{RUNTIME}", "^1.0.0"],
  "@example/plugin@3.0.0": ["{RUNTIME}", 3],
  "@example/plugin@4.0.0": "not a list"
}}"#
    );
    let file = parse_compatibility(FILENAME, Some(&text));
    assert_eq!(
        file.exemptions.keys().collect::<Vec<_>>(),
        vec!["@example/plugin@1.0.0"]
    );
    assert_eq!(file.warnings.len(), 5);
    assert!(file.warnings[0].starts_with(FILENAME));
    assert!(!file.rewritable);
}

#[test]
fn a_file_the_reader_accepted_is_rewritable() {
    let text = format!(r#"{{"@example/plugin@1.0.0": ["{RUNTIME}"]}}"#);
    let file = parse_compatibility(FILENAME, Some(&text));
    assert_eq!(file.exemptions.len(), 1);
    assert!(file.warnings.is_empty());
    assert!(file.rewritable);
}

#[test]
fn an_exemption_requires_two_exact_identities() {
    for (package, runtime) in [
        ("@example/plugin@1.0.0", RUNTIME),
        ("plugin@1.0.0+build", "2.0.0-rc.1"),
    ] {
        assert_eq!(validate_plugin_version_exemption(package, runtime), Ok(()));
    }
    for (package, runtime) in [
        ("@example/plugin", RUNTIME),
        ("@example/plugin@^1.0.0", RUNTIME),
        ("/plugins/dsh-x", RUNTIME),
        ("@example/plugin@1.0.0", "^1.0.0"),
        ("@example/plugin@1.0.0", "not-a-version"),
        ("Bad Name@1.0.0", RUNTIME),
    ] {
        assert_eq!(
            validate_plugin_version_exemption(package, runtime),
            Err(ExemptionError::InvalidIdentity),
            "{package} / {runtime}"
        );
    }
}

/// An accepted, empty record.
fn empty_file() -> CompatibilityFile {
    CompatibilityFile {
        exemptions: BTreeMap::new(),
        warnings: Vec::new(),
        rewritable: true,
    }
}

#[test]
fn a_grant_names_one_runtime_and_requires_acknowledged_risk() {
    let granted = update_profile_compatibility(
        &empty_file(),
        "@example/plugin@1.0.0",
        "2.0.0",
        true,
        true,
        "2.0.0",
    )
    .expect("granted");
    assert_eq!(
        granted.get("@example/plugin@1.0.0"),
        Some(&vec!["2.0.0".to_owned()])
    );
    // Granting the same pair twice is one entry, not two.
    let mut file = empty_file();
    file.exemptions = granted;
    let again =
        update_profile_compatibility(&file, "@example/plugin@1.0.0", "2.0.0", true, true, "2.0.0")
            .expect("granted");
    assert_eq!(again, file.exemptions);
}

#[test]
fn a_grant_without_consent_stale_writes_or_an_unrepaired_file_is_refused() {
    assert_eq!(
        update_profile_compatibility(
            &empty_file(),
            "@example/plugin@1.0.0",
            "2.0.0",
            true,
            false,
            "2.0.0",
        ),
        Err(ExemptionError::RiskNotAcknowledged)
    );
    assert_eq!(
        update_profile_compatibility(
            &empty_file(),
            "@example/plugin@1.0.0",
            "1.0.0",
            true,
            true,
            "2.0.0",
        ),
        Err(ExemptionError::StaleRuntime {
            requested: "1.0.0".to_owned(),
            current: "2.0.0".to_owned(),
        })
    );
    let unrepairable = CompatibilityFile {
        exemptions: BTreeMap::new(),
        warnings: vec!["broken record".to_owned()],
        rewritable: false,
    };
    assert!(matches!(
        update_profile_compatibility(
            &unrepairable,
            "@example/plugin@1.0.0",
            "2.0.0",
            true,
            true,
            "2.0.0",
        ),
        Err(ExemptionError::RepairRequired { .. })
    ));
    // A revocation may name a historical runtime: refusing to withdraw a grant is not a safety
    // property, and leaving one in place because the runtime moved on is the failure mode.
    assert_eq!(
        update_profile_compatibility(
            &empty_file(),
            "@example/plugin@1.0.0",
            "1.0.0",
            false,
            false,
            "2.0.0",
        ),
        Ok(BTreeMap::new())
    );
}

#[test]
fn a_revocation_drops_one_runtime_and_removes_the_key_with_its_last_version() {
    let mut file = empty_file();
    file.exemptions.insert(
        "@example/plugin@1.0.0".to_owned(),
        vec!["1.0.0".to_owned(), "2.0.0".to_owned()],
    );
    let revoked = update_profile_compatibility(
        &file,
        "@example/plugin@1.0.0",
        "1.0.0",
        false,
        false,
        "2.0.0",
    )
    .expect("revoked");
    assert_eq!(
        revoked.get("@example/plugin@1.0.0"),
        Some(&vec!["2.0.0".to_owned()])
    );
    let emptied = update_profile_compatibility(
        &file,
        "@example/plugin@1.0.0",
        "1.0.0",
        false,
        false,
        "2.0.0",
    )
    .and_then(|single| {
        update_profile_compatibility(
            &CompatibilityFile {
                exemptions: single,
                ..empty_file()
            },
            "@example/plugin@1.0.0",
            "2.0.0",
            false,
            false,
            "2.0.0",
        )
    })
    .expect("revoked");
    assert!(emptied.is_empty());
}

#[test]
fn the_written_document_sorts_keys_and_round_trips() {
    let mut exemptions = BTreeMap::new();
    exemptions.insert(
        "@example/plugin@1.0.0".to_owned(),
        vec!["1.0.0".to_owned(), "2.0.0".to_owned()],
    );
    exemptions.insert("@example/other@1.0.0".to_owned(), vec!["1.0.0".to_owned()]);
    let text = render_compatibility(&exemptions);
    assert!(text.ends_with("}\n"));
    assert!(text.contains("  \"@example/other@1.0.0\": [\"1.0.0\"],\n"));
    assert!(text.contains("  \"@example/plugin@1.0.0\": [\"1.0.0\", \"2.0.0\"]\n"));
    // What is written is exactly what the reader accepts back.
    let read = parse_compatibility(FILENAME, Some(&text));
    assert!(read.warnings.is_empty(), "{:?}", read.warnings);
    assert!(read.rewritable);
    assert_eq!(read.exemptions, exemptions);
    // The empty document is still valid JSON and still rewritable.
    assert_eq!(render_compatibility(&BTreeMap::new()), "{\n}\n");
}

// ---- host admission ------------------------------------------------------------------------

#[test]
fn only_a_linux_host_provided_by_windows_is_wsl() {
    let facts = |platform: &str,
                 distro: Option<&str>,
                 interop: Option<&str>,
                 release: Option<&str>| HostFacts {
        platform: platform.to_owned(),
        wsl_distro_name: distro.map(str::to_owned),
        wsl_interop: interop.map(str::to_owned),
        os_release: release.map(str::to_owned),
    };
    assert!(is_wsl(&facts("linux", Some("Ubuntu"), None, None)));
    assert!(is_wsl(&facts("linux", None, Some("/run/WSL/1"), None)));
    assert!(is_wsl(&facts(
        "linux",
        None,
        None,
        Some("5.15.90.1-microsoft-standard-WSL2")
    )));
    // An empty variable is not evidence of WSL.
    assert!(!is_wsl(&facts("linux", Some(""), Some(""), Some(""))));
    // Only a Linux host can be WSL, however the environment is set.
    assert!(!is_wsl(&facts(
        "darwin",
        Some("Ubuntu"),
        None,
        Some("microsoft")
    )));
    assert!(!is_wsl(&facts("linux", None, None, Some("6.1.0-generic"))));
}

/// The facts of a Linux host that is not WSL.
fn plain_linux() -> HostFacts {
    HostFacts {
        platform: "linux".to_owned(),
        ..HostFacts::default()
    }
}

#[test]
fn a_platform_declaration_is_a_nonempty_list_of_known_names() {
    assert_eq!(
        declared_platforms(&json!(["linux", "wsl"])),
        Ok(vec!["linux".to_owned(), "wsl".to_owned()])
    );
    for malformed in [
        json!([]),
        json!("linux"),
        json!(["linux", 1]),
        json!(["solaris"]),
        json!(null),
    ] {
        assert_eq!(
            declared_platforms(&malformed),
            Err(MalformedPlatforms),
            "{malformed}"
        );
    }
}

#[test]
fn omission_admits_every_platform_and_a_declaration_excludes_the_rest() {
    assert_eq!(assert_bundle_platform(None, &plain_linux()), Ok(()));
    assert_eq!(
        assert_bundle_platform(Some(&json!(["linux", "darwin"])), &plain_linux()),
        Ok(())
    );
    // A malformed declaration is refused rather than ignored: ignoring it admits the bundle on the
    // very host it was meant to exclude.
    assert!(matches!(
        assert_bundle_platform(Some(&json!("linux")), &plain_linux()),
        Err(PlatformError::Malformed(_))
    ));
    assert!(matches!(
        assert_bundle_platform(Some(&json!(["darwin"])), &plain_linux()),
        Err(PlatformError::Unsupported(_))
    ));
    // `wsl` admits a Windows host only when the host really is WSL.
    let wsl = HostFacts {
        platform: "linux".to_owned(),
        wsl_distro_name: Some("Ubuntu".to_owned()),
        ..HostFacts::default()
    };
    assert_eq!(assert_bundle_platform(Some(&json!(["wsl"])), &wsl), Ok(()));
    assert!(matches!(
        assert_bundle_platform(Some(&json!(["wsl"])), &plain_linux()),
        Err(PlatformError::Unsupported(_))
    ));
    match assert_bundle_platform(Some(&json!(["darwin", "win32"])), &plain_linux()) {
        Err(PlatformError::Unsupported(unsupported)) => {
            assert_eq!(unsupported.platforms, "darwin, win32");
            assert_eq!(unsupported.platform, "linux");
            assert_eq!(
                unsupported.to_string(),
                "Bundle supports Host platforms darwin, win32; the DSH Host is running linux"
            );
        }
        other => panic!("{other:?}"),
    }
}


// Path aliases so a file compiled with #[path] still resolves crate::<domain>::<module>.
mod boot {
    pub use super::boot_gap as install;
}
