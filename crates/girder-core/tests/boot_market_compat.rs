//! Tests for `market_compat_gap`: the profile projection the plugin market puts over an index
//! plugin — whether the running DSH accepts each version, whether the profile already has the
//! package, which version a plain install takes, and which versions a Git copy can update to.
//!
//! The three functions under test are upstream `packages/boot/plugin-market/src/index.ts`'s
//! `listingOf`, `installationOf` and `compatibilityOf`, plus the `node-semver` range evaluator
//! `compatibilityOf` needs and `boot_gap` deliberately does not provide.
//!
//! # Where the expectations come from
//!
//! Two suites here are **differential against the real upstream code**, not against a reading of
//! it:
//!
//! - [`satisfies_matches_node_semver_for_every_recorded_range`] replays 824 ranges — every
//!   operator over every shape of version, hyphen ranges, `||` alternatives, `v`/`=` prefixes,
//!   and 55 malformed inputs — across 35 versions, comparing all 28,840 answers with
//!   `semver.satisfies(v, r, { includePrerelease: true })` from `node-semver` 7.8.5, the version
//!   this repository depends on.
//! - [`listing_matches_upstream_for_every_recorded_projection`] replays 136 projections of
//!   upstream's `listingOf`/`installationOf`/`compatibilityOf` bodies, run against the *real*
//!   `evaluatePluginCompatibility`, `pinnedGitOrigin` and `marketInstallTarget` from this
//!   repository's compiled `packages/boot/app-boot`, `packages/boot/plugin-manager` and
//!   `packages/util/plugin-market-index`, over 21 plugins × 21 bundle states × 4 exemption maps
//!   × 4 running versions. The compared values are the default version, the installation status
//!   and its installed version, every version's install spec and registry, every compatibility
//!   verdict, and every update flag.
//!
//! The rest state the *reason* a case exists rather than a recording: a listing never claims a
//! version runs when its range is unmet, an exemption records consent without pretending the
//! range was met, and a local copy that cannot be ordered is never reported as outdated.
//!
//! # Why the module is compiled by path
//!
//! The crate root's wiring is a separate change, so this compiles the exact source files rather
//! than copies. Inside a `#[path]` module the `crate::` prefix names *this* test crate, whose root
//! declares `boot_gap` and `plugin_market_gap` the same way — so `market_compat_gap`'s own
//! `use crate::boot::install::…` and `use crate::boot::plugin_market::…` resolve identically here and in
//! the library.

#[path = "../src/boot/install.rs"]
pub mod boot_gap;
#[path = "../src/boot/market_compat.rs"]
pub mod market_compat_gap;
#[path = "../src/boot/plugin_market.rs"]
pub mod plugin_market_gap;

use std::collections::BTreeMap;

use market_compat_gap::{
    BundleAvailability, BundleFacts, BundleUpdate, ENGINES_DSH, MarketCompatibility,
    MarketInstallation, MarketInstallationStatus, compatibility_of, installation_of, listing_of,
    satisfies,
};
use plugin_market_gap::{
    MarketEngines, MarketPlugin, MarketPluginMetadata, MarketSource, MarketVersion,
};

// ============================================================================================
// Helpers
// ============================================================================================

/// The repository every Git version fixture installs from.
const GIT_URL: &str = "https://git.example/o/plugins";

/// The running DSH versions every recorded projection is replayed against.
const RUNTIMES: [&str; 4] = ["2.1.0", "1.0.0", "2.0.0-rc.1", "0.0.1"];

/// A full 40-hex commit name, from one repeated digit.
fn commit(digit: &str) -> String {
    digit.repeat(40)
}

/// One index version.
fn version(version: &str, source: MarketSource, engines: MarketEngines) -> MarketVersion {
    MarketVersion {
        version: version.to_owned(),
        publish_timestamp: 0,
        source,
        engines,
    }
}

/// An `engines.dsh` declaration.
fn engines(range: &str) -> MarketEngines {
    MarketEngines {
        dsh: Some(range.to_owned()),
    }
}

/// An index plugin with only the fields these projections read.
fn plugin(name: &str, versions: Vec<MarketVersion>) -> MarketPlugin {
    MarketPlugin {
        name: name.to_owned(),
        title: None,
        description: None,
        icon: None,
        author: None,
        homepage: None,
        repository: None,
        metadata: MarketPluginMetadata::default(),
        versions,
    }
}

/// A profile bundle, with every unread field at its default.
fn bundle(name: &str) -> BundleFacts {
    BundleFacts {
        name: name.to_owned(),
        installed: true,
        ..BundleFacts::default()
    }
}

/// An npm version with no registry and no requirements.
fn npm(v: &str) -> MarketVersion {
    version(
        v,
        MarketSource::Npm { registry: None },
        MarketEngines::default(),
    )
}

/// The fixture plugin whose name matches.
fn fixture(name: &str) -> MarketPlugin {
    plugins()
        .into_iter()
        .find(|plugin| plugin.name == name)
        .unwrap_or_else(|| panic!("no fixture plugin named {name:?}"))
}

// ============================================================================================
// Fixtures — the exact inputs the upstream oracle was run over
// ============================================================================================

/// The index plugins, in the order the recorded table refers to them.
#[allow(
    clippy::too_many_lines,
    reason = "one fixture table: every recorded projection refers to these plugins by position in it, so splitting the list would separate a row from the entry it names"
)]
fn plugins() -> Vec<MarketPlugin> {
    vec![
        plugin(
            "plain",
            vec![version(
                "1.0.0",
                MarketSource::Npm { registry: None },
                MarketEngines::default(),
            )],
        ),
        plugin(
            "engines-ok",
            vec![version(
                "1.0.0",
                MarketSource::Npm { registry: None },
                engines("^1.0.0 || ^2.0.0"),
            )],
        ),
        plugin(
            "engines-bad",
            vec![version(
                "1.0.0",
                MarketSource::Npm { registry: None },
                engines("0.0.1"),
            )],
        ),
        plugin(
            "engines-reversed",
            vec![version(
                "1.0.0",
                MarketSource::Npm { registry: None },
                engines("^1.0.0"),
            )],
        ),
        plugin(
            "engines-prerelease",
            vec![version(
                "1.0.0",
                MarketSource::Npm { registry: None },
                engines(">=2.0.0-rc.1"),
            )],
        ),
        plugin(
            "engines-invalid",
            vec![version(
                "1.0.0",
                MarketSource::Npm { registry: None },
                engines("not-a-range"),
            )],
        ),
        plugin(
            "engines-empty",
            vec![version(
                "1.0.0",
                MarketSource::Npm { registry: None },
                engines(""),
            )],
        ),
        plugin(
            "engines-blank",
            vec![version(
                "1.0.0",
                MarketSource::Npm { registry: None },
                engines("   "),
            )],
        ),
        plugin(
            "engines-tilde",
            vec![version(
                "1.0.0",
                MarketSource::Npm { registry: None },
                engines("~2.1"),
            )],
        ),
        plugin(
            "engines-hyphen",
            vec![version(
                "1.0.0",
                MarketSource::Npm { registry: None },
                engines("2.0.0 - 2.2.0"),
            )],
        ),
        plugin(
            "engines-wildcard",
            vec![version(
                "1.0.0",
                MarketSource::Npm { registry: None },
                engines("*"),
            )],
        ),
        plugin(
            "mixed",
            vec![
                version(
                    "2.0.0-rc.1",
                    MarketSource::Npm { registry: None },
                    MarketEngines::default(),
                ),
                version(
                    "1.0.0",
                    MarketSource::Npm { registry: None },
                    MarketEngines::default(),
                ),
                version(
                    "1.1.0",
                    MarketSource::Npm { registry: None },
                    engines("0.0.1"),
                ),
            ],
        ),
        plugin(
            "previews",
            vec![
                version(
                    "1.0.0-beta.1",
                    MarketSource::Npm { registry: None },
                    MarketEngines::default(),
                ),
                version(
                    "1.0.0-rc.1",
                    MarketSource::Npm { registry: None },
                    engines("0.0.1"),
                ),
            ],
        ),
        plugin(
            "only-preview-stable",
            vec![
                version(
                    "2.0.0-rc.1",
                    MarketSource::Npm { registry: None },
                    MarketEngines::default(),
                ),
                version(
                    "1.0.0",
                    MarketSource::Npm { registry: None },
                    engines("0.0.1"),
                ),
            ],
        ),
        plugin(
            "all-incompatible",
            vec![
                version(
                    "1.0.0",
                    MarketSource::Npm { registry: None },
                    engines("0.0.1"),
                ),
                version(
                    "2.0.0",
                    MarketSource::Npm { registry: None },
                    engines("0.0.1"),
                ),
            ],
        ),
        plugin(
            "single-prerelease",
            vec![version(
                "1.0.0-rc.1",
                MarketSource::Npm { registry: None },
                MarketEngines::default(),
            )],
        ),
        plugin(
            "registry",
            vec![version(
                "1.0.0",
                MarketSource::Npm {
                    registry: Some("https://r.example/".to_owned()),
                },
                engines("0.0.1"),
            )],
        ),
        plugin(
            "git-versions",
            vec![
                version(
                    "1.3.0",
                    MarketSource::Git {
                        url: GIT_URL.to_owned(),
                        commit: commit("d"),
                        path: Some("packages/q".to_owned()),
                    },
                    MarketEngines::default(),
                ),
                version(
                    "1.2.0",
                    MarketSource::Git {
                        url: GIT_URL.to_owned(),
                        commit: commit("c"),
                        path: Some("packages/p".to_owned()),
                    },
                    MarketEngines::default(),
                ),
                version(
                    "1.1.0",
                    MarketSource::Npm { registry: None },
                    MarketEngines::default(),
                ),
                version(
                    "1.0.0",
                    MarketSource::Git {
                        url: GIT_URL.to_owned(),
                        commit: commit("a"),
                        path: None,
                    },
                    MarketEngines::default(),
                ),
                version(
                    "0.9.0",
                    MarketSource::Git {
                        url: GIT_URL.to_owned(),
                        commit: commit("9"),
                        path: None,
                    },
                    MarketEngines::default(),
                ),
            ],
        ),
        plugin(
            "same-origin",
            vec![
                version(
                    "1.2.0",
                    MarketSource::Git {
                        url: GIT_URL.to_owned(),
                        commit: commit("c"),
                        path: Some("packages/p".to_owned()),
                    },
                    MarketEngines::default(),
                ),
                version(
                    "1.0.0",
                    MarketSource::Git {
                        url: GIT_URL.to_owned(),
                        commit: commit("a"),
                        path: Some("packages/p".to_owned()),
                    },
                    MarketEngines::default(),
                ),
            ],
        ),
        plugin(
            "hosted",
            vec![version(
                "1.1.0",
                MarketSource::Git {
                    url: "https://github.com/o/plugins".to_owned(),
                    commit: commit("b"),
                    path: Some("p".to_owned()),
                },
                MarketEngines::default(),
            )],
        ),
        plugin(
            "many",
            vec![
                version(
                    "3.0.0",
                    MarketSource::Npm { registry: None },
                    MarketEngines::default(),
                ),
                version(
                    "2.0.0-rc.1",
                    MarketSource::Npm { registry: None },
                    MarketEngines::default(),
                ),
                version(
                    "1.5.0",
                    MarketSource::Npm { registry: None },
                    engines("^1.0.0"),
                ),
                version(
                    "1.0.0",
                    MarketSource::Npm { registry: None },
                    MarketEngines::default(),
                ),
            ],
        ),
    ]
}

/// The profile states the oracle was run with. Each is a whole bundle list, so `[]` really means
/// "the profile lists nothing".
#[allow(
    clippy::too_many_lines,
    reason = "one fixture table: every recorded projection refers to a bundle state by position in it, so splitting the list would separate a row from the state it names"
)]
fn bundles() -> [Vec<BundleFacts>; 21] {
    [
        vec![
        ],
        vec![
            BundleFacts {
                name: "plain".to_owned(),
                installed: true,
                availability: BundleAvailability::Profile,
                version: Some("1.0.0".to_owned()),
                update: None,
                source: None,
            },
        ],
        vec![
            BundleFacts {
                name: "plain".to_owned(),
                installed: true,
                availability: BundleAvailability::Profile,
                version: Some("0.5.0".to_owned()),
                update: None,
                source: None,
            },
        ],
        vec![
            BundleFacts {
                name: "plain".to_owned(),
                installed: true,
                availability: BundleAvailability::Profile,
                version: Some("9.0.0".to_owned()),
                update: None,
                source: None,
            },
        ],
        vec![
            BundleFacts {
                name: "plain".to_owned(),
                installed: true,
                availability: BundleAvailability::Profile,
                version: None,
                update: None,
                source: None,
            },
        ],
        vec![
            BundleFacts {
                name: "plain".to_owned(),
                installed: true,
                availability: BundleAvailability::Profile,
                version: Some("local".to_owned()),
                update: None,
                source: None,
            },
        ],
        vec![
            BundleFacts {
                name: "plain".to_owned(),
                installed: false,
                availability: BundleAvailability::Installation,
                version: Some("1.0.0".to_owned()),
                update: None,
                source: None,
            },
        ],
        vec![
            BundleFacts {
                name: "plain".to_owned(),
                installed: false,
                availability: BundleAvailability::Installation,
                version: None,
                update: None,
                source: None,
            },
        ],
        vec![
            BundleFacts {
                name: "plain".to_owned(),
                installed: false,
                availability: BundleAvailability::Missing,
                version: None,
                update: None,
                source: None,
            },
        ],
        vec![
            BundleFacts {
                name: "plain".to_owned(),
                installed: true,
                availability: BundleAvailability::Profile,
                version: Some("1.0.0".to_owned()),
                update: Some(BundleUpdate::Git),
                source: Some("git+ssh://git@git.example/o/plugins.git#aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa&path:/packages/p".to_owned()),
            },
        ],
        vec![
            BundleFacts {
                name: "git-versions".to_owned(),
                installed: true,
                availability: BundleAvailability::Profile,
                version: Some("1.0.0".to_owned()),
                update: Some(BundleUpdate::Git),
                source: Some("git+ssh://git@git.example/o/plugins.git#aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa&path:/packages/p".to_owned()),
            },
        ],
        vec![
            BundleFacts {
                name: "git-versions".to_owned(),
                installed: true,
                availability: BundleAvailability::Profile,
                version: Some("1.0.0".to_owned()),
                update: Some(BundleUpdate::Registry),
                source: Some("git-versions@1.0.0".to_owned()),
            },
        ],
        vec![
            BundleFacts {
                name: "git-versions".to_owned(),
                installed: true,
                availability: BundleAvailability::Profile,
                version: Some("1.0.0".to_owned()),
                update: Some(BundleUpdate::Git),
                source: None,
            },
        ],
        vec![
            BundleFacts {
                name: "git-versions".to_owned(),
                installed: true,
                availability: BundleAvailability::Profile,
                version: None,
                update: Some(BundleUpdate::Git),
                source: Some("git+ssh://git@git.example/o/plugins.git#aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa&path:/packages/p".to_owned()),
            },
        ],
        vec![
            BundleFacts {
                name: "git-versions".to_owned(),
                installed: true,
                availability: BundleAvailability::Profile,
                version: Some("1.0.0".to_owned()),
                update: Some(BundleUpdate::Git),
                source: Some("git+https://git.example/o/plugins.git#main&path:/packages/p".to_owned()),
            },
        ],
        vec![
            BundleFacts {
                name: "mixed".to_owned(),
                installed: true,
                availability: BundleAvailability::Profile,
                version: Some("1.0.0".to_owned()),
                update: None,
                source: None,
            },
        ],
        vec![
            BundleFacts {
                name: "many".to_owned(),
                installed: true,
                availability: BundleAvailability::Profile,
                version: Some("1.5.0".to_owned()),
                update: None,
                source: None,
            },
        ],
        vec![
            BundleFacts {
                name: "same-origin".to_owned(),
                installed: true,
                availability: BundleAvailability::Profile,
                version: Some("1.0.0".to_owned()),
                update: Some(BundleUpdate::Git),
                source: Some("git+ssh://git@git.example/o/plugins.git#aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa&path:/packages/p".to_owned()),
            },
        ],
        vec![
            BundleFacts {
                name: "hosted".to_owned(),
                installed: true,
                availability: BundleAvailability::Profile,
                version: Some("1.0.0".to_owned()),
                update: Some(BundleUpdate::Git),
                source: Some("github:o/plugins#aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa&path:/p".to_owned()),
            },
        ],
        vec![
            BundleFacts {
                name: "same-origin".to_owned(),
                installed: true,
                availability: BundleAvailability::Profile,
                version: Some("1.1.0".to_owned()),
                update: Some(BundleUpdate::Git),
                source: Some("git+https://git.example/o/plugins.git#bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb&path:/packages/p".to_owned()),
            },
        ],
        vec![
            BundleFacts {
                name: "other".to_owned(),
                installed: true,
                availability: BundleAvailability::Profile,
                version: Some("1.0.0".to_owned()),
                update: None,
                source: None,
            },
        ],
    ]
}

fn exemption_maps() -> [BTreeMap<String, Vec<String>>; 4] {
    [
        BTreeMap::new(),
        BTreeMap::from([("engines-bad@1.0.0".to_owned(), vec!["2.1.0".to_owned()])]),
        BTreeMap::from([("engines-bad@1.0.0".to_owned(), vec!["9.9.9".to_owned()])]),
        BTreeMap::from([("engines-bad@1.0.0".to_owned(), vec![])]),
    ]
}

/// The recorded upstream projections, one per line:
/// `plugin | bundle | exemptions | version set | compatibility | exempted | update | outcomes`.
///
/// Each mask group holds one hexadecimal digit per running version, in [`RUNTIMES`] order, with bit
/// *n* belonging to listed version *n*. Each outcome is `defaultVersion|status|installedVersion`.
const PROJECTIONS: [&str; 136] = [
    "plain|0|0|0|1,1,1,1|0,0,0,0|0,0,0,0|1.0.0|not-installed|-,1.0.0|not-installed|-,1.0.0|not-installed|-,1.0.0|not-installed|-",
    "plain|0|1|0|1,1,1,1|0,0,0,0|0,0,0,0|1.0.0|not-installed|-,1.0.0|not-installed|-,1.0.0|not-installed|-,1.0.0|not-installed|-",
    "plain|0|2|0|1,1,1,1|0,0,0,0|0,0,0,0|1.0.0|not-installed|-,1.0.0|not-installed|-,1.0.0|not-installed|-,1.0.0|not-installed|-",
    "plain|0|3|0|1,1,1,1|0,0,0,0|0,0,0,0|1.0.0|not-installed|-,1.0.0|not-installed|-,1.0.0|not-installed|-,1.0.0|not-installed|-",
    "plain|1|0|0|1,1,1,1|0,0,0,0|0,0,0,0|1.0.0|installed|1.0.0,1.0.0|installed|1.0.0,1.0.0|installed|1.0.0,1.0.0|installed|1.0.0",
    "plain|1|1|0|1,1,1,1|0,0,0,0|0,0,0,0|1.0.0|installed|1.0.0,1.0.0|installed|1.0.0,1.0.0|installed|1.0.0,1.0.0|installed|1.0.0",
    "plain|1|2|0|1,1,1,1|0,0,0,0|0,0,0,0|1.0.0|installed|1.0.0,1.0.0|installed|1.0.0,1.0.0|installed|1.0.0,1.0.0|installed|1.0.0",
    "plain|1|3|0|1,1,1,1|0,0,0,0|0,0,0,0|1.0.0|installed|1.0.0,1.0.0|installed|1.0.0,1.0.0|installed|1.0.0,1.0.0|installed|1.0.0",
    "plain|2|0|0|1,1,1,1|0,0,0,0|0,0,0,0|1.0.0|update-available|0.5.0,1.0.0|update-available|0.5.0,1.0.0|update-available|0.5.0,1.0.0|update-available|0.5.0",
    "plain|2|1|0|1,1,1,1|0,0,0,0|0,0,0,0|1.0.0|update-available|0.5.0,1.0.0|update-available|0.5.0,1.0.0|update-available|0.5.0,1.0.0|update-available|0.5.0",
    "plain|2|2|0|1,1,1,1|0,0,0,0|0,0,0,0|1.0.0|update-available|0.5.0,1.0.0|update-available|0.5.0,1.0.0|update-available|0.5.0,1.0.0|update-available|0.5.0",
    "plain|2|3|0|1,1,1,1|0,0,0,0|0,0,0,0|1.0.0|update-available|0.5.0,1.0.0|update-available|0.5.0,1.0.0|update-available|0.5.0,1.0.0|update-available|0.5.0",
    "plain|3|0|0|1,1,1,1|0,0,0,0|0,0,0,0|1.0.0|different-version|9.0.0,1.0.0|different-version|9.0.0,1.0.0|different-version|9.0.0,1.0.0|different-version|9.0.0",
    "plain|3|1|0|1,1,1,1|0,0,0,0|0,0,0,0|1.0.0|different-version|9.0.0,1.0.0|different-version|9.0.0,1.0.0|different-version|9.0.0,1.0.0|different-version|9.0.0",
    "plain|3|2|0|1,1,1,1|0,0,0,0|0,0,0,0|1.0.0|different-version|9.0.0,1.0.0|different-version|9.0.0,1.0.0|different-version|9.0.0,1.0.0|different-version|9.0.0",
    "plain|3|3|0|1,1,1,1|0,0,0,0|0,0,0,0|1.0.0|different-version|9.0.0,1.0.0|different-version|9.0.0,1.0.0|different-version|9.0.0,1.0.0|different-version|9.0.0",
    "plain|4|0|0|1,1,1,1|0,0,0,0|0,0,0,0|1.0.0|different-version|-,1.0.0|different-version|-,1.0.0|different-version|-,1.0.0|different-version|-",
    "plain|4|1|0|1,1,1,1|0,0,0,0|0,0,0,0|1.0.0|different-version|-,1.0.0|different-version|-,1.0.0|different-version|-,1.0.0|different-version|-",
    "plain|4|2|0|1,1,1,1|0,0,0,0|0,0,0,0|1.0.0|different-version|-,1.0.0|different-version|-,1.0.0|different-version|-,1.0.0|different-version|-",
    "plain|4|3|0|1,1,1,1|0,0,0,0|0,0,0,0|1.0.0|different-version|-,1.0.0|different-version|-,1.0.0|different-version|-,1.0.0|different-version|-",
    "plain|5|0|0|1,1,1,1|0,0,0,0|0,0,0,0|1.0.0|different-version|local,1.0.0|different-version|local,1.0.0|different-version|local,1.0.0|different-version|local",
    "plain|5|1|0|1,1,1,1|0,0,0,0|0,0,0,0|1.0.0|different-version|local,1.0.0|different-version|local,1.0.0|different-version|local,1.0.0|different-version|local",
    "plain|5|2|0|1,1,1,1|0,0,0,0|0,0,0,0|1.0.0|different-version|local,1.0.0|different-version|local,1.0.0|different-version|local,1.0.0|different-version|local",
    "plain|5|3|0|1,1,1,1|0,0,0,0|0,0,0,0|1.0.0|different-version|local,1.0.0|different-version|local,1.0.0|different-version|local,1.0.0|different-version|local",
    "plain|6|0|0|1,1,1,1|0,0,0,0|0,0,0,0|1.0.0|provided|1.0.0,1.0.0|provided|1.0.0,1.0.0|provided|1.0.0,1.0.0|provided|1.0.0",
    "plain|6|1|0|1,1,1,1|0,0,0,0|0,0,0,0|1.0.0|provided|1.0.0,1.0.0|provided|1.0.0,1.0.0|provided|1.0.0,1.0.0|provided|1.0.0",
    "plain|6|2|0|1,1,1,1|0,0,0,0|0,0,0,0|1.0.0|provided|1.0.0,1.0.0|provided|1.0.0,1.0.0|provided|1.0.0,1.0.0|provided|1.0.0",
    "plain|6|3|0|1,1,1,1|0,0,0,0|0,0,0,0|1.0.0|provided|1.0.0,1.0.0|provided|1.0.0,1.0.0|provided|1.0.0,1.0.0|provided|1.0.0",
    "plain|7|0|0|1,1,1,1|0,0,0,0|0,0,0,0|1.0.0|provided|-,1.0.0|provided|-,1.0.0|provided|-,1.0.0|provided|-",
    "plain|7|1|0|1,1,1,1|0,0,0,0|0,0,0,0|1.0.0|provided|-,1.0.0|provided|-,1.0.0|provided|-,1.0.0|provided|-",
    "plain|7|2|0|1,1,1,1|0,0,0,0|0,0,0,0|1.0.0|provided|-,1.0.0|provided|-,1.0.0|provided|-,1.0.0|provided|-",
    "plain|7|3|0|1,1,1,1|0,0,0,0|0,0,0,0|1.0.0|provided|-,1.0.0|provided|-,1.0.0|provided|-,1.0.0|provided|-",
    "engines-ok|0|0|1|1,1,0,0|0,0,0,0|0,0,0,0|1.0.0|not-installed|-,1.0.0|not-installed|-,-|not-installed|-,-|not-installed|-",
    "engines-ok|0|1|1|1,1,0,0|0,0,0,0|0,0,0,0|1.0.0|not-installed|-,1.0.0|not-installed|-,-|not-installed|-,-|not-installed|-",
    "engines-ok|0|2|1|1,1,0,0|0,0,0,0|0,0,0,0|1.0.0|not-installed|-,1.0.0|not-installed|-,-|not-installed|-,-|not-installed|-",
    "engines-ok|0|3|1|1,1,0,0|0,0,0,0|0,0,0,0|1.0.0|not-installed|-,1.0.0|not-installed|-,-|not-installed|-,-|not-installed|-",
    "engines-bad|0|0|2|0,0,0,1|0,0,0,0|0,0,0,0|-|not-installed|-,-|not-installed|-,-|not-installed|-,1.0.0|not-installed|-",
    "engines-bad|0|1|2|0,0,0,1|1,0,0,0|0,0,0,0|-|not-installed|-,-|not-installed|-,-|not-installed|-,1.0.0|not-installed|-",
    "engines-bad|0|2|2|0,0,0,1|0,0,0,0|0,0,0,0|-|not-installed|-,-|not-installed|-,-|not-installed|-,1.0.0|not-installed|-",
    "engines-bad|0|3|2|0,0,0,1|0,0,0,0|0,0,0,0|-|not-installed|-,-|not-installed|-,-|not-installed|-,1.0.0|not-installed|-",
    "engines-reversed|0|0|3|0,1,0,0|0,0,0,0|0,0,0,0|-|not-installed|-,1.0.0|not-installed|-,-|not-installed|-,-|not-installed|-",
    "engines-reversed|0|1|3|0,1,0,0|0,0,0,0|0,0,0,0|-|not-installed|-,1.0.0|not-installed|-,-|not-installed|-,-|not-installed|-",
    "engines-reversed|0|2|3|0,1,0,0|0,0,0,0|0,0,0,0|-|not-installed|-,1.0.0|not-installed|-,-|not-installed|-,-|not-installed|-",
    "engines-reversed|0|3|3|0,1,0,0|0,0,0,0|0,0,0,0|-|not-installed|-,1.0.0|not-installed|-,-|not-installed|-,-|not-installed|-",
    "engines-prerelease|0|0|4|1,0,1,0|0,0,0,0|0,0,0,0|1.0.0|not-installed|-,-|not-installed|-,1.0.0|not-installed|-,-|not-installed|-",
    "engines-prerelease|0|1|4|1,0,1,0|0,0,0,0|0,0,0,0|1.0.0|not-installed|-,-|not-installed|-,1.0.0|not-installed|-,-|not-installed|-",
    "engines-prerelease|0|2|4|1,0,1,0|0,0,0,0|0,0,0,0|1.0.0|not-installed|-,-|not-installed|-,1.0.0|not-installed|-,-|not-installed|-",
    "engines-prerelease|0|3|4|1,0,1,0|0,0,0,0|0,0,0,0|1.0.0|not-installed|-,-|not-installed|-,1.0.0|not-installed|-,-|not-installed|-",
    "engines-invalid|0|0|5|0,0,0,0|0,0,0,0|0,0,0,0|-|not-installed|-,-|not-installed|-,-|not-installed|-,-|not-installed|-",
    "engines-invalid|0|1|5|0,0,0,0|0,0,0,0|0,0,0,0|-|not-installed|-,-|not-installed|-,-|not-installed|-,-|not-installed|-",
    "engines-invalid|0|2|5|0,0,0,0|0,0,0,0|0,0,0,0|-|not-installed|-,-|not-installed|-,-|not-installed|-,-|not-installed|-",
    "engines-invalid|0|3|5|0,0,0,0|0,0,0,0|0,0,0,0|-|not-installed|-,-|not-installed|-,-|not-installed|-,-|not-installed|-",
    "engines-empty|0|0|6|0,0,0,0|0,0,0,0|0,0,0,0|-|not-installed|-,-|not-installed|-,-|not-installed|-,-|not-installed|-",
    "engines-empty|0|1|6|0,0,0,0|0,0,0,0|0,0,0,0|-|not-installed|-,-|not-installed|-,-|not-installed|-,-|not-installed|-",
    "engines-empty|0|2|6|0,0,0,0|0,0,0,0|0,0,0,0|-|not-installed|-,-|not-installed|-,-|not-installed|-,-|not-installed|-",
    "engines-empty|0|3|6|0,0,0,0|0,0,0,0|0,0,0,0|-|not-installed|-,-|not-installed|-,-|not-installed|-,-|not-installed|-",
    "engines-blank|0|0|7|0,0,0,0|0,0,0,0|0,0,0,0|-|not-installed|-,-|not-installed|-,-|not-installed|-,-|not-installed|-",
    "engines-blank|0|1|7|0,0,0,0|0,0,0,0|0,0,0,0|-|not-installed|-,-|not-installed|-,-|not-installed|-,-|not-installed|-",
    "engines-blank|0|2|7|0,0,0,0|0,0,0,0|0,0,0,0|-|not-installed|-,-|not-installed|-,-|not-installed|-,-|not-installed|-",
    "engines-blank|0|3|7|0,0,0,0|0,0,0,0|0,0,0,0|-|not-installed|-,-|not-installed|-,-|not-installed|-,-|not-installed|-",
    "engines-tilde|0|0|8|1,0,0,0|0,0,0,0|0,0,0,0|1.0.0|not-installed|-,-|not-installed|-,-|not-installed|-,-|not-installed|-",
    "engines-tilde|0|1|8|1,0,0,0|0,0,0,0|0,0,0,0|1.0.0|not-installed|-,-|not-installed|-,-|not-installed|-,-|not-installed|-",
    "engines-tilde|0|2|8|1,0,0,0|0,0,0,0|0,0,0,0|1.0.0|not-installed|-,-|not-installed|-,-|not-installed|-,-|not-installed|-",
    "engines-tilde|0|3|8|1,0,0,0|0,0,0,0|0,0,0,0|1.0.0|not-installed|-,-|not-installed|-,-|not-installed|-,-|not-installed|-",
    "engines-hyphen|0|0|9|1,0,1,0|0,0,0,0|0,0,0,0|1.0.0|not-installed|-,-|not-installed|-,1.0.0|not-installed|-,-|not-installed|-",
    "engines-hyphen|0|1|9|1,0,1,0|0,0,0,0|0,0,0,0|1.0.0|not-installed|-,-|not-installed|-,1.0.0|not-installed|-,-|not-installed|-",
    "engines-hyphen|0|2|9|1,0,1,0|0,0,0,0|0,0,0,0|1.0.0|not-installed|-,-|not-installed|-,1.0.0|not-installed|-,-|not-installed|-",
    "engines-hyphen|0|3|9|1,0,1,0|0,0,0,0|0,0,0,0|1.0.0|not-installed|-,-|not-installed|-,1.0.0|not-installed|-,-|not-installed|-",
    "engines-wildcard|0|0|10|1,1,1,1|0,0,0,0|0,0,0,0|1.0.0|not-installed|-,1.0.0|not-installed|-,1.0.0|not-installed|-,1.0.0|not-installed|-",
    "engines-wildcard|0|1|10|1,1,1,1|0,0,0,0|0,0,0,0|1.0.0|not-installed|-,1.0.0|not-installed|-,1.0.0|not-installed|-,1.0.0|not-installed|-",
    "engines-wildcard|0|2|10|1,1,1,1|0,0,0,0|0,0,0,0|1.0.0|not-installed|-,1.0.0|not-installed|-,1.0.0|not-installed|-,1.0.0|not-installed|-",
    "engines-wildcard|0|3|10|1,1,1,1|0,0,0,0|0,0,0,0|1.0.0|not-installed|-,1.0.0|not-installed|-,1.0.0|not-installed|-,1.0.0|not-installed|-",
    "mixed|0|0|11|3,3,3,7|0,0,0,0|0,0,0,0|1.0.0|not-installed|-,1.0.0|not-installed|-,1.0.0|not-installed|-,1.0.0|not-installed|-",
    "mixed|0|1|11|3,3,3,7|0,0,0,0|0,0,0,0|1.0.0|not-installed|-,1.0.0|not-installed|-,1.0.0|not-installed|-,1.0.0|not-installed|-",
    "mixed|0|2|11|3,3,3,7|0,0,0,0|0,0,0,0|1.0.0|not-installed|-,1.0.0|not-installed|-,1.0.0|not-installed|-,1.0.0|not-installed|-",
    "mixed|0|3|11|3,3,3,7|0,0,0,0|0,0,0,0|1.0.0|not-installed|-,1.0.0|not-installed|-,1.0.0|not-installed|-,1.0.0|not-installed|-",
    "previews|0|0|12|1,1,1,3|0,0,0,0|0,0,0,0|1.0.0-beta.1|not-installed|-,1.0.0-beta.1|not-installed|-,1.0.0-beta.1|not-installed|-,1.0.0-beta.1|not-installed|-",
    "previews|0|1|12|1,1,1,3|0,0,0,0|0,0,0,0|1.0.0-beta.1|not-installed|-,1.0.0-beta.1|not-installed|-,1.0.0-beta.1|not-installed|-,1.0.0-beta.1|not-installed|-",
    "previews|0|2|12|1,1,1,3|0,0,0,0|0,0,0,0|1.0.0-beta.1|not-installed|-,1.0.0-beta.1|not-installed|-,1.0.0-beta.1|not-installed|-,1.0.0-beta.1|not-installed|-",
    "previews|0|3|12|1,1,1,3|0,0,0,0|0,0,0,0|1.0.0-beta.1|not-installed|-,1.0.0-beta.1|not-installed|-,1.0.0-beta.1|not-installed|-,1.0.0-beta.1|not-installed|-",
    "only-preview-stable|0|0|13|1,1,1,3|0,0,0,0|0,0,0,0|-|not-installed|-,-|not-installed|-,-|not-installed|-,1.0.0|not-installed|-",
    "only-preview-stable|0|1|13|1,1,1,3|0,0,0,0|0,0,0,0|-|not-installed|-,-|not-installed|-,-|not-installed|-,1.0.0|not-installed|-",
    "only-preview-stable|0|2|13|1,1,1,3|0,0,0,0|0,0,0,0|-|not-installed|-,-|not-installed|-,-|not-installed|-,1.0.0|not-installed|-",
    "only-preview-stable|0|3|13|1,1,1,3|0,0,0,0|0,0,0,0|-|not-installed|-,-|not-installed|-,-|not-installed|-,1.0.0|not-installed|-",
    "all-incompatible|0|0|14|0,0,0,3|0,0,0,0|0,0,0,0|-|not-installed|-,-|not-installed|-,-|not-installed|-,1.0.0|not-installed|-",
    "all-incompatible|0|1|14|0,0,0,3|0,0,0,0|0,0,0,0|-|not-installed|-,-|not-installed|-,-|not-installed|-,1.0.0|not-installed|-",
    "all-incompatible|0|2|14|0,0,0,3|0,0,0,0|0,0,0,0|-|not-installed|-,-|not-installed|-,-|not-installed|-,1.0.0|not-installed|-",
    "all-incompatible|0|3|14|0,0,0,3|0,0,0,0|0,0,0,0|-|not-installed|-,-|not-installed|-,-|not-installed|-,1.0.0|not-installed|-",
    "single-prerelease|0|0|15|1,1,1,1|0,0,0,0|0,0,0,0|1.0.0-rc.1|not-installed|-,1.0.0-rc.1|not-installed|-,1.0.0-rc.1|not-installed|-,1.0.0-rc.1|not-installed|-",
    "single-prerelease|0|1|15|1,1,1,1|0,0,0,0|0,0,0,0|1.0.0-rc.1|not-installed|-,1.0.0-rc.1|not-installed|-,1.0.0-rc.1|not-installed|-,1.0.0-rc.1|not-installed|-",
    "single-prerelease|0|2|15|1,1,1,1|0,0,0,0|0,0,0,0|1.0.0-rc.1|not-installed|-,1.0.0-rc.1|not-installed|-,1.0.0-rc.1|not-installed|-,1.0.0-rc.1|not-installed|-",
    "single-prerelease|0|3|15|1,1,1,1|0,0,0,0|0,0,0,0|1.0.0-rc.1|not-installed|-,1.0.0-rc.1|not-installed|-,1.0.0-rc.1|not-installed|-,1.0.0-rc.1|not-installed|-",
    "registry|0|0|16|0,0,0,1|0,0,0,0|0,0,0,0|-|not-installed|-,-|not-installed|-,-|not-installed|-,1.0.0|not-installed|-",
    "registry|0|1|16|0,0,0,1|0,0,0,0|0,0,0,0|-|not-installed|-,-|not-installed|-,-|not-installed|-,1.0.0|not-installed|-",
    "registry|0|2|16|0,0,0,1|0,0,0,0|0,0,0,0|-|not-installed|-,-|not-installed|-,-|not-installed|-,1.0.0|not-installed|-",
    "registry|0|3|16|0,0,0,1|0,0,0,0|0,0,0,0|-|not-installed|-,-|not-installed|-,-|not-installed|-,1.0.0|not-installed|-",
    "git-versions|0|0|17|1f,1f,1f,1f|0,0,0,0|0,0,0,0|1.3.0|not-installed|-,1.3.0|not-installed|-,1.3.0|not-installed|-,1.3.0|not-installed|-",
    "git-versions|0|1|17|1f,1f,1f,1f|0,0,0,0|0,0,0,0|1.3.0|not-installed|-,1.3.0|not-installed|-,1.3.0|not-installed|-,1.3.0|not-installed|-",
    "git-versions|0|2|17|1f,1f,1f,1f|0,0,0,0|0,0,0,0|1.3.0|not-installed|-,1.3.0|not-installed|-,1.3.0|not-installed|-,1.3.0|not-installed|-",
    "git-versions|0|3|17|1f,1f,1f,1f|0,0,0,0|0,0,0,0|1.3.0|not-installed|-,1.3.0|not-installed|-,1.3.0|not-installed|-,1.3.0|not-installed|-",
    "git-versions|10|0|17|1f,1f,1f,1f|0,0,0,0|2,2,2,2|1.3.0|update-available|1.0.0,1.3.0|update-available|1.0.0,1.3.0|update-available|1.0.0,1.3.0|update-available|1.0.0",
    "git-versions|10|1|17|1f,1f,1f,1f|0,0,0,0|2,2,2,2|1.3.0|update-available|1.0.0,1.3.0|update-available|1.0.0,1.3.0|update-available|1.0.0,1.3.0|update-available|1.0.0",
    "git-versions|10|2|17|1f,1f,1f,1f|0,0,0,0|2,2,2,2|1.3.0|update-available|1.0.0,1.3.0|update-available|1.0.0,1.3.0|update-available|1.0.0,1.3.0|update-available|1.0.0",
    "git-versions|10|3|17|1f,1f,1f,1f|0,0,0,0|2,2,2,2|1.3.0|update-available|1.0.0,1.3.0|update-available|1.0.0,1.3.0|update-available|1.0.0,1.3.0|update-available|1.0.0",
    "git-versions|11|0|17|1f,1f,1f,1f|0,0,0,0|0,0,0,0|1.3.0|update-available|1.0.0,1.3.0|update-available|1.0.0,1.3.0|update-available|1.0.0,1.3.0|update-available|1.0.0",
    "git-versions|11|1|17|1f,1f,1f,1f|0,0,0,0|0,0,0,0|1.3.0|update-available|1.0.0,1.3.0|update-available|1.0.0,1.3.0|update-available|1.0.0,1.3.0|update-available|1.0.0",
    "git-versions|11|2|17|1f,1f,1f,1f|0,0,0,0|0,0,0,0|1.3.0|update-available|1.0.0,1.3.0|update-available|1.0.0,1.3.0|update-available|1.0.0,1.3.0|update-available|1.0.0",
    "git-versions|11|3|17|1f,1f,1f,1f|0,0,0,0|0,0,0,0|1.3.0|update-available|1.0.0,1.3.0|update-available|1.0.0,1.3.0|update-available|1.0.0,1.3.0|update-available|1.0.0",
    "git-versions|13|0|17|1f,1f,1f,1f|0,0,0,0|0,0,0,0|1.3.0|different-version|-,1.3.0|different-version|-,1.3.0|different-version|-,1.3.0|different-version|-",
    "git-versions|13|1|17|1f,1f,1f,1f|0,0,0,0|0,0,0,0|1.3.0|different-version|-,1.3.0|different-version|-,1.3.0|different-version|-,1.3.0|different-version|-",
    "git-versions|13|2|17|1f,1f,1f,1f|0,0,0,0|0,0,0,0|1.3.0|different-version|-,1.3.0|different-version|-,1.3.0|different-version|-,1.3.0|different-version|-",
    "git-versions|13|3|17|1f,1f,1f,1f|0,0,0,0|0,0,0,0|1.3.0|different-version|-,1.3.0|different-version|-,1.3.0|different-version|-,1.3.0|different-version|-",
    "same-origin|0|0|18|3,3,3,3|0,0,0,0|0,0,0,0|1.2.0|not-installed|-,1.2.0|not-installed|-,1.2.0|not-installed|-,1.2.0|not-installed|-",
    "same-origin|0|1|18|3,3,3,3|0,0,0,0|0,0,0,0|1.2.0|not-installed|-,1.2.0|not-installed|-,1.2.0|not-installed|-,1.2.0|not-installed|-",
    "same-origin|0|2|18|3,3,3,3|0,0,0,0|0,0,0,0|1.2.0|not-installed|-,1.2.0|not-installed|-,1.2.0|not-installed|-,1.2.0|not-installed|-",
    "same-origin|0|3|18|3,3,3,3|0,0,0,0|0,0,0,0|1.2.0|not-installed|-,1.2.0|not-installed|-,1.2.0|not-installed|-,1.2.0|not-installed|-",
    "same-origin|17|0|18|3,3,3,3|0,0,0,0|1,1,1,1|1.2.0|update-available|1.0.0,1.2.0|update-available|1.0.0,1.2.0|update-available|1.0.0,1.2.0|update-available|1.0.0",
    "same-origin|17|1|18|3,3,3,3|0,0,0,0|1,1,1,1|1.2.0|update-available|1.0.0,1.2.0|update-available|1.0.0,1.2.0|update-available|1.0.0,1.2.0|update-available|1.0.0",
    "same-origin|17|2|18|3,3,3,3|0,0,0,0|1,1,1,1|1.2.0|update-available|1.0.0,1.2.0|update-available|1.0.0,1.2.0|update-available|1.0.0,1.2.0|update-available|1.0.0",
    "same-origin|17|3|18|3,3,3,3|0,0,0,0|1,1,1,1|1.2.0|update-available|1.0.0,1.2.0|update-available|1.0.0,1.2.0|update-available|1.0.0,1.2.0|update-available|1.0.0",
    "same-origin|19|0|18|3,3,3,3|0,0,0,0|1,1,1,1|1.2.0|update-available|1.1.0,1.2.0|update-available|1.1.0,1.2.0|update-available|1.1.0,1.2.0|update-available|1.1.0",
    "same-origin|19|1|18|3,3,3,3|0,0,0,0|1,1,1,1|1.2.0|update-available|1.1.0,1.2.0|update-available|1.1.0,1.2.0|update-available|1.1.0,1.2.0|update-available|1.1.0",
    "same-origin|19|2|18|3,3,3,3|0,0,0,0|1,1,1,1|1.2.0|update-available|1.1.0,1.2.0|update-available|1.1.0,1.2.0|update-available|1.1.0,1.2.0|update-available|1.1.0",
    "same-origin|19|3|18|3,3,3,3|0,0,0,0|1,1,1,1|1.2.0|update-available|1.1.0,1.2.0|update-available|1.1.0,1.2.0|update-available|1.1.0,1.2.0|update-available|1.1.0",
    "hosted|0|0|19|1,1,1,1|0,0,0,0|0,0,0,0|1.1.0|not-installed|-,1.1.0|not-installed|-,1.1.0|not-installed|-,1.1.0|not-installed|-",
    "hosted|0|1|19|1,1,1,1|0,0,0,0|0,0,0,0|1.1.0|not-installed|-,1.1.0|not-installed|-,1.1.0|not-installed|-,1.1.0|not-installed|-",
    "hosted|0|2|19|1,1,1,1|0,0,0,0|0,0,0,0|1.1.0|not-installed|-,1.1.0|not-installed|-,1.1.0|not-installed|-,1.1.0|not-installed|-",
    "hosted|0|3|19|1,1,1,1|0,0,0,0|0,0,0,0|1.1.0|not-installed|-,1.1.0|not-installed|-,1.1.0|not-installed|-,1.1.0|not-installed|-",
    "hosted|18|0|19|1,1,1,1|0,0,0,0|1,1,1,1|1.1.0|update-available|1.0.0,1.1.0|update-available|1.0.0,1.1.0|update-available|1.0.0,1.1.0|update-available|1.0.0",
    "hosted|18|1|19|1,1,1,1|0,0,0,0|1,1,1,1|1.1.0|update-available|1.0.0,1.1.0|update-available|1.0.0,1.1.0|update-available|1.0.0,1.1.0|update-available|1.0.0",
    "hosted|18|2|19|1,1,1,1|0,0,0,0|1,1,1,1|1.1.0|update-available|1.0.0,1.1.0|update-available|1.0.0,1.1.0|update-available|1.0.0,1.1.0|update-available|1.0.0",
    "hosted|18|3|19|1,1,1,1|0,0,0,0|1,1,1,1|1.1.0|update-available|1.0.0,1.1.0|update-available|1.0.0,1.1.0|update-available|1.0.0,1.1.0|update-available|1.0.0",
    "many|0|0|20|b,f,b,b|0,0,0,0|0,0,0,0|3.0.0|not-installed|-,3.0.0|not-installed|-,3.0.0|not-installed|-,3.0.0|not-installed|-",
    "many|0|1|20|b,f,b,b|0,0,0,0|0,0,0,0|3.0.0|not-installed|-,3.0.0|not-installed|-,3.0.0|not-installed|-,3.0.0|not-installed|-",
    "many|0|2|20|b,f,b,b|0,0,0,0|0,0,0,0|3.0.0|not-installed|-,3.0.0|not-installed|-,3.0.0|not-installed|-,3.0.0|not-installed|-",
    "many|0|3|20|b,f,b,b|0,0,0,0|0,0,0,0|3.0.0|not-installed|-,3.0.0|not-installed|-,3.0.0|not-installed|-,3.0.0|not-installed|-",
];

/// One version set as `(version, install spec, registry)` — the install targets every runtime
/// must agree on, because they come from the index alone.
const VERSION_SETS: [&[(&str, &str, Option<&str>)]; 21] = [
    &[("1.0.0", "plain@1.0.0", None)],
    &[("1.0.0", "engines-ok@1.0.0", None)],
    &[("1.0.0", "engines-bad@1.0.0", None)],
    &[("1.0.0", "engines-reversed@1.0.0", None)],
    &[("1.0.0", "engines-prerelease@1.0.0", None)],
    &[("1.0.0", "engines-invalid@1.0.0", None)],
    &[("1.0.0", "engines-empty@1.0.0", None)],
    &[("1.0.0", "engines-blank@1.0.0", None)],
    &[("1.0.0", "engines-tilde@1.0.0", None)],
    &[("1.0.0", "engines-hyphen@1.0.0", None)],
    &[("1.0.0", "engines-wildcard@1.0.0", None)],
    &[
        ("2.0.0-rc.1", "mixed@2.0.0-rc.1", None),
        ("1.0.0", "mixed@1.0.0", None),
        ("1.1.0", "mixed@1.1.0", None),
    ],
    &[
        ("1.0.0-beta.1", "previews@1.0.0-beta.1", None),
        ("1.0.0-rc.1", "previews@1.0.0-rc.1", None),
    ],
    &[
        ("2.0.0-rc.1", "only-preview-stable@2.0.0-rc.1", None),
        ("1.0.0", "only-preview-stable@1.0.0", None),
    ],
    &[
        ("1.0.0", "all-incompatible@1.0.0", None),
        ("2.0.0", "all-incompatible@2.0.0", None),
    ],
    &[("1.0.0-rc.1", "single-prerelease@1.0.0-rc.1", None)],
    &[("1.0.0", "registry@1.0.0", Some("https://r.example/"))],
    &[
        (
            "1.3.0",
            "git+https://git.example/o/plugins.git#dddddddddddddddddddddddddddddddddddddddd&path:/packages/q",
            None,
        ),
        (
            "1.2.0",
            "git+https://git.example/o/plugins.git#cccccccccccccccccccccccccccccccccccccccc&path:/packages/p",
            None,
        ),
        ("1.1.0", "git-versions@1.1.0", None),
        (
            "1.0.0",
            "git+https://git.example/o/plugins.git#aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
            None,
        ),
        (
            "0.9.0",
            "git+https://git.example/o/plugins.git#9999999999999999999999999999999999999999",
            None,
        ),
    ],
    &[
        (
            "1.2.0",
            "git+https://git.example/o/plugins.git#cccccccccccccccccccccccccccccccccccccccc&path:/packages/p",
            None,
        ),
        (
            "1.0.0",
            "git+https://git.example/o/plugins.git#aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa&path:/packages/p",
            None,
        ),
    ],
    &[(
        "1.1.0",
        "git+https://github.com/o/plugins.git#bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb&path:/p",
        None,
    )],
    &[
        ("3.0.0", "many@3.0.0", None),
        ("2.0.0-rc.1", "many@2.0.0-rc.1", None),
        ("1.5.0", "many@1.5.0", None),
        ("1.0.0", "many@1.0.0", None),
    ],
];

/// 824 ranges × 35 versions, as `semver.satisfies(v, r, { includePrerelease: true })` in Node.
///
/// `0x` followed by one bit per entry of [`SATISFYING_VERSIONS`], in order.
static RANGE_TABLE: [(&str, u64); 824] = [
    ("0", 0x4_0000_007f),
    ("=0", 0x4_0000_007f),
    ("^0", 0x4_0000_007f),
    ("~0", 0x4_0000_007f),
    ("~>0", 0x4_0000_007f),
    (">0", 0x3_ffff_ff80),
    (">=0", 0x7_ffff_ffff),
    ("<0", 0x0),
    ("<=0", 0x4_0000_007f),
    ("v0", 0x4_0000_007f),
    ("=v0", 0x4_0000_007f),
    ("0.0", 0x4_0000_0007),
    ("=0.0", 0x4_0000_0007),
    ("^0.0", 0x4_0000_0007),
    ("~0.0", 0x4_0000_0007),
    ("~>0.0", 0x4_0000_0007),
    (">0.0", 0x3_ffff_fff8),
    (">=0.0", 0x7_ffff_ffff),
    ("<0.0", 0x0),
    ("<=0.0", 0x4_0000_0007),
    ("v0.0", 0x4_0000_0007),
    ("=v0.0", 0x4_0000_0007),
    ("0.0.0", 0x1),
    ("=0.0.0", 0x1),
    ("^0.0.0", 0x1),
    ("~0.0.0", 0x7),
    ("~>0.0.0", 0x7),
    (">0.0.0", 0x3_ffff_fffe),
    (">=0.0.0", 0x3_ffff_ffff),
    ("<0.0.0", 0x4_0000_0000),
    ("<=0.0.0", 0x4_0000_0001),
    ("v0.0.0", 0x1),
    ("=v0.0.0", 0x1),
    ("0.0.1", 0x2),
    ("=0.0.1", 0x2),
    ("^0.0.1", 0x2),
    ("~0.0.1", 0x6),
    ("~>0.0.1", 0x6),
    (">0.0.1", 0x3_ffff_fffc),
    (">=0.0.1", 0x3_ffff_fffe),
    ("<0.0.1", 0x4_0000_0001),
    ("<=0.0.1", 0x4_0000_0003),
    ("v0.0.1", 0x2),
    ("=v0.0.1", 0x2),
    ("0.1", 0x18),
    ("=0.1", 0x18),
    ("^0.1", 0x18),
    ("~0.1", 0x18),
    ("~>0.1", 0x18),
    (">0.1", 0x3_ffff_ffe0),
    (">=0.1", 0x3_ffff_fff8),
    ("<0.1", 0x4_0000_0007),
    ("<=0.1", 0x4_0000_001f),
    ("v0.1", 0x18),
    ("=v0.1", 0x18),
    ("0.1.0", 0x8),
    ("=0.1.0", 0x8),
    ("^0.1.0", 0x18),
    ("~0.1.0", 0x18),
    ("~>0.1.0", 0x18),
    (">0.1.0", 0x3_ffff_fff0),
    (">=0.1.0", 0x3_ffff_fff8),
    ("<0.1.0", 0x4_0000_0007),
    ("<=0.1.0", 0x4_0000_000f),
    ("v0.1.0", 0x8),
    ("=v0.1.0", 0x8),
    ("1", 0x2_01ff_ff80),
    ("=1", 0x2_01ff_ff80),
    ("^1", 0x2_01ff_ff80),
    ("~1", 0x2_01ff_ff80),
    ("~>1", 0x2_01ff_ff80),
    (">1", 0x1_fe00_0000),
    (">=1", 0x3_ffff_ff80),
    ("<1", 0x4_0000_007f),
    ("<=1", 0x6_01ff_ffff),
    ("v1", 0x2_01ff_ff80),
    ("=v1", 0x2_01ff_ff80),
    ("1.0", 0x3f80),
    ("=1.0", 0x3f80),
    ("^1.0", 0x2_01ff_ff80),
    ("~1.0", 0x3f80),
    ("~>1.0", 0x3f80),
    (">1.0", 0x3_ffff_c000),
    (">=1.0", 0x3_ffff_ff80),
    ("<1.0", 0x4_0000_007f),
    ("<=1.0", 0x4_0000_3fff),
    ("v1.0", 0x3f80),
    ("=v1.0", 0x3f80),
    ("1.0.0", 0x80),
    ("=1.0.0", 0x80),
    ("^1.0.0", 0x2_01ff_c080),
    ("~1.0.0", 0x80),
    ("~>1.0.0", 0x80),
    (">1.0.0", 0x3_ffff_c000),
    (">=1.0.0", 0x3_ffff_c080),
    ("<1.0.0", 0x4_0000_3f7f),
    ("<=1.0.0", 0x4_0000_3fff),
    ("v1.0.0", 0x80),
    ("=v1.0.0", 0x80),
    ("1.2", 0x2_007f_8000),
    ("=1.2", 0x2_007f_8000),
    ("^1.2", 0x2_01ff_8000),
    ("~1.2", 0x2_007f_8000),
    ("~>1.2", 0x2_007f_8000),
    (">1.2", 0x1_ff80_0000),
    (">=1.2", 0x3_ffff_8000),
    ("<1.2", 0x4_0000_7fff),
    ("<=1.2", 0x6_007f_ffff),
    ("v1.2", 0x2_007f_8000),
    ("=v1.2", 0x2_007f_8000),
    ("1.2.3", 0x2_0002_0000),
    ("=1.2.3", 0x2_0002_0000),
    ("^1.2.3", 0x2_01c2_0000),
    ("~1.2.3", 0x2_0042_0000),
    ("~>1.2.3", 0x2_0042_0000),
    (">1.2.3", 0x1_ffc0_0000),
    (">=1.2.3", 0x3_ffc2_0000),
    ("<1.2.3", 0x4_003d_ffff),
    ("<=1.2.3", 0x6_003f_ffff),
    ("v1.2.3", 0x2_0002_0000),
    ("=v1.2.3", 0x2_0002_0000),
    ("1.2.3-alpha", 0x8_0000),
    ("=1.2.3-alpha", 0x8_0000),
    ("^1.2.3-alpha", 0x2_01fa_0000),
    ("~1.2.3-alpha", 0x2_007a_0000),
    ("~>1.2.3-alpha", 0x2_007a_0000),
    (">1.2.3-alpha", 0x3_fff2_0000),
    (">=1.2.3-alpha", 0x3_fffa_0000),
    ("<1.2.3-alpha", 0x4_0005_ffff),
    ("<=1.2.3-alpha", 0x4_000d_ffff),
    ("v1.2.3-alpha", 0x8_0000),
    ("=v1.2.3-alpha", 0x8_0000),
    ("1.2.3-alpha.1", 0x10_0000),
    ("=1.2.3-alpha.1", 0x10_0000),
    ("^1.2.3-alpha.1", 0x2_01f2_0000),
    ("~1.2.3-alpha.1", 0x2_0072_0000),
    ("~>1.2.3-alpha.1", 0x2_0072_0000),
    (">1.2.3-alpha.1", 0x3_ffe2_0000),
    (">=1.2.3-alpha.1", 0x3_fff2_0000),
    ("<1.2.3-alpha.1", 0x4_000d_ffff),
    ("<=1.2.3-alpha.1", 0x4_001d_ffff),
    ("v1.2.3-alpha.1", 0x10_0000),
    ("=v1.2.3-alpha.1", 0x10_0000),
    ("1.x", 0x2_01ff_ff80),
    ("=1.x", 0x2_01ff_ff80),
    ("^1.x", 0x2_01ff_ff80),
    ("~1.x", 0x2_01ff_ff80),
    ("~>1.x", 0x2_01ff_ff80),
    (">1.x", 0x1_fe00_0000),
    (">=1.x", 0x3_ffff_ff80),
    ("<1.x", 0x4_0000_007f),
    ("<=1.x", 0x6_01ff_ffff),
    ("v1.x", 0x2_01ff_ff80),
    ("=v1.x", 0x2_01ff_ff80),
    ("1.2.x", 0x2_007f_8000),
    ("=1.2.x", 0x2_007f_8000),
    ("^1.2.x", 0x2_01ff_8000),
    ("~1.2.x", 0x2_007f_8000),
    ("~>1.2.x", 0x2_007f_8000),
    (">1.2.x", 0x1_ff80_0000),
    (">=1.2.x", 0x3_ffff_8000),
    ("<1.2.x", 0x4_0000_7fff),
    ("<=1.2.x", 0x6_007f_ffff),
    ("v1.2.x", 0x2_007f_8000),
    ("=v1.2.x", 0x2_007f_8000),
    ("1.X", 0x2_01ff_ff80),
    ("=1.X", 0x2_01ff_ff80),
    ("^1.X", 0x2_01ff_ff80),
    ("~1.X", 0x2_01ff_ff80),
    ("~>1.X", 0x2_01ff_ff80),
    (">1.X", 0x1_fe00_0000),
    (">=1.X", 0x3_ffff_ff80),
    ("<1.X", 0x4_0000_007f),
    ("<=1.X", 0x6_01ff_ffff),
    ("v1.X", 0x2_01ff_ff80),
    ("=v1.X", 0x2_01ff_ff80),
    ("1.*", 0x2_01ff_ff80),
    ("=1.*", 0x2_01ff_ff80),
    ("^1.*", 0x2_01ff_ff80),
    ("~1.*", 0x2_01ff_ff80),
    ("~>1.*", 0x2_01ff_ff80),
    (">1.*", 0x1_fe00_0000),
    (">=1.*", 0x3_ffff_ff80),
    ("<1.*", 0x4_0000_007f),
    ("<=1.*", 0x6_01ff_ffff),
    ("v1.*", 0x2_01ff_ff80),
    ("=v1.*", 0x2_01ff_ff80),
    ("*", 0x7_ffff_ffff),
    ("=*", 0x7_ffff_ffff),
    ("^*", 0x7_ffff_ffff),
    ("~*", 0x7_ffff_ffff),
    ("~>*", 0x7_ffff_ffff),
    (">*", 0x0),
    (">=*", 0x7_ffff_ffff),
    ("<*", 0x0),
    ("<=*", 0x7_ffff_ffff),
    ("v*", 0x7_ffff_ffff),
    ("=v*", 0x7_ffff_ffff),
    ("x", 0x7_ffff_ffff),
    ("=x", 0x7_ffff_ffff),
    ("^x", 0x7_ffff_ffff),
    ("~x", 0x7_ffff_ffff),
    ("~>x", 0x7_ffff_ffff),
    (">x", 0x0),
    (">=x", 0x7_ffff_ffff),
    ("<x", 0x0),
    ("<=x", 0x7_ffff_ffff),
    ("vx", 0x7_ffff_ffff),
    ("=vx", 0x7_ffff_ffff),
    ("X", 0x7_ffff_ffff),
    ("=X", 0x7_ffff_ffff),
    ("^X", 0x7_ffff_ffff),
    ("~X", 0x7_ffff_ffff),
    ("~>X", 0x7_ffff_ffff),
    (">X", 0x0),
    (">=X", 0x7_ffff_ffff),
    ("<X", 0x0),
    ("<=X", 0x7_ffff_ffff),
    ("vX", 0x7_ffff_ffff),
    ("=vX", 0x7_ffff_ffff),
    ("x.x", 0x7_ffff_ffff),
    ("=x.x", 0x7_ffff_ffff),
    ("^x.x", 0x7_ffff_ffff),
    ("~x.x", 0x7_ffff_ffff),
    ("~>x.x", 0x7_ffff_ffff),
    (">x.x", 0x0),
    (">=x.x", 0x7_ffff_ffff),
    ("<x.x", 0x0),
    ("<=x.x", 0x7_ffff_ffff),
    ("vx.x", 0x7_ffff_ffff),
    ("=vx.x", 0x7_ffff_ffff),
    ("1.x.x", 0x2_01ff_ff80),
    ("=1.x.x", 0x2_01ff_ff80),
    ("^1.x.x", 0x2_01ff_ff80),
    ("~1.x.x", 0x2_01ff_ff80),
    ("~>1.x.x", 0x2_01ff_ff80),
    (">1.x.x", 0x1_fe00_0000),
    (">=1.x.x", 0x3_ffff_ff80),
    ("<1.x.x", 0x4_0000_007f),
    ("<=1.x.x", 0x6_01ff_ffff),
    ("v1.x.x", 0x2_01ff_ff80),
    ("=v1.x.x", 0x2_01ff_ff80),
    ("*.*.*", 0x7_ffff_ffff),
    ("=*.*.*", 0x7_ffff_ffff),
    ("^*.*.*", 0x7_ffff_ffff),
    ("~*.*.*", 0x7_ffff_ffff),
    ("~>*.*.*", 0x7_ffff_ffff),
    (">*.*.*", 0x0),
    (">=*.*.*", 0x7_ffff_ffff),
    ("<*.*.*", 0x0),
    ("<=*.*.*", 0x7_ffff_ffff),
    ("v*.*.*", 0x7_ffff_ffff),
    ("=v*.*.*", 0x7_ffff_ffff),
    ("2", 0x7e00_0000),
    ("=2", 0x7e00_0000),
    ("^2", 0x7e00_0000),
    ("~2", 0x7e00_0000),
    ("~>2", 0x7e00_0000),
    (">2", 0x1_8000_0000),
    (">=2", 0x1_fe00_0000),
    ("<2", 0x6_01ff_ffff),
    ("<=2", 0x6_7fff_ffff),
    ("v2", 0x7e00_0000),
    ("=v2", 0x7e00_0000),
    ("2.0", 0x1e00_0000),
    ("=2.0", 0x1e00_0000),
    ("^2.0", 0x7e00_0000),
    ("~2.0", 0x1e00_0000),
    ("~>2.0", 0x1e00_0000),
    (">2.0", 0x1_e000_0000),
    (">=2.0", 0x1_fe00_0000),
    ("<2.0", 0x6_01ff_ffff),
    ("<=2.0", 0x6_1fff_ffff),
    ("v2.0", 0x1e00_0000),
    ("=v2.0", 0x1e00_0000),
    ("2.0.0-0", 0x400_0000),
    ("=2.0.0-0", 0x400_0000),
    ("^2.0.0-0", 0x7e00_0000),
    ("~2.0.0-0", 0x1e00_0000),
    ("~>2.0.0-0", 0x1e00_0000),
    (">2.0.0-0", 0x1_fa00_0000),
    (">=2.0.0-0", 0x1_fe00_0000),
    ("<2.0.0-0", 0x6_01ff_ffff),
    ("<=2.0.0-0", 0x6_05ff_ffff),
    ("v2.0.0-0", 0x400_0000),
    ("=v2.0.0-0", 0x400_0000),
    ("2.0.0-rc.1", 0x800_0000),
    ("=2.0.0-rc.1", 0x800_0000),
    ("^2.0.0-rc.1", 0x7a00_0000),
    ("~2.0.0-rc.1", 0x1a00_0000),
    ("~>2.0.0-rc.1", 0x1a00_0000),
    (">2.0.0-rc.1", 0x1_f200_0000),
    (">=2.0.0-rc.1", 0x1_fa00_0000),
    ("<2.0.0-rc.1", 0x6_05ff_ffff),
    ("<=2.0.0-rc.1", 0x6_0dff_ffff),
    ("v2.0.0-rc.1", 0x800_0000),
    ("=v2.0.0-rc.1", 0x800_0000),
    ("1.2.3+build", 0x2_0002_0000),
    ("=1.2.3+build", 0x2_0002_0000),
    ("^1.2.3+build", 0x2_01c2_0000),
    ("~1.2.3+build", 0x2_0042_0000),
    ("~>1.2.3+build", 0x2_0042_0000),
    (">1.2.3+build", 0x1_ffc0_0000),
    (">=1.2.3+build", 0x3_ffc2_0000),
    ("<1.2.3+build", 0x4_003d_ffff),
    ("<=1.2.3+build", 0x6_003f_ffff),
    ("v1.2.3+build", 0x2_0002_0000),
    ("=v1.2.3+build", 0x2_0002_0000),
    ("1.2 - 2.3.4", 0x2_3fff_8000),
    ("1.2 - 2.3", 0x2_3fff_8000),
    ("1.2 - 2", 0x2_7fff_8000),
    ("1.2 - 1.5.0", 0x2_00ff_8000),
    ("1.2 - 2.0.0", 0x2_0fff_8000),
    ("1.2 - 2.0.0-rc.1", 0x2_0dff_8000),
    ("1.2 - 1.2.3", 0x2_003f_8000),
    ("1.2 - 1.3", 0x2_00ff_8000),
    ("1.2 - x", 0x3_ffff_8000),
    ("1.2 - *", 0x3_ffff_8000),
    ("1.2 - 3.0.0-0", 0x2_7fff_8000),
    ("1.2.0 - 2.3.4", 0x2_3fff_8000),
    ("1.2.0 - 2.3", 0x2_3fff_8000),
    ("1.2.0 - 2", 0x2_7fff_8000),
    ("1.2.0 - 1.5.0", 0x2_00ff_8000),
    ("1.2.0 - 2.0.0", 0x2_0fff_8000),
    ("1.2.0 - 2.0.0-rc.1", 0x2_0dff_8000),
    ("1.2.0 - 1.2.3", 0x2_003f_8000),
    ("1.2.0 - 1.3", 0x2_00ff_8000),
    ("1.2.0 - x", 0x3_ffff_8000),
    ("1.2.0 - *", 0x3_ffff_8000),
    ("1.2.0 - 3.0.0-0", 0x2_7fff_8000),
    ("1.0.0 - 2.3.4", 0x2_3fff_ff80),
    ("1.0.0 - 2.3", 0x2_3fff_ff80),
    ("1.0.0 - 2", 0x2_7fff_ff80),
    ("1.0.0 - 1.5.0", 0x2_00ff_ff80),
    ("1.0.0 - 2.0.0", 0x2_0fff_ff80),
    ("1.0.0 - 2.0.0-rc.1", 0x2_0dff_ff80),
    ("1.0.0 - 1.2.3", 0x2_003f_ff80),
    ("1.0.0 - 1.3", 0x2_00ff_ff80),
    ("1.0.0 - x", 0x3_ffff_ff80),
    ("1.0.0 - *", 0x3_ffff_ff80),
    ("1.0.0 - 3.0.0-0", 0x2_7fff_ff80),
    ("0.1 - 2.3.4", 0x2_3fff_fff8),
    ("0.1 - 2.3", 0x2_3fff_fff8),
    ("0.1 - 2", 0x2_7fff_fff8),
    ("0.1 - 1.5.0", 0x2_00ff_fff8),
    ("0.1 - 2.0.0", 0x2_0fff_fff8),
    ("0.1 - 2.0.0-rc.1", 0x2_0dff_fff8),
    ("0.1 - 1.2.3", 0x2_003f_fff8),
    ("0.1 - 1.3", 0x2_00ff_fff8),
    ("0.1 - x", 0x3_ffff_fff8),
    ("0.1 - *", 0x3_ffff_fff8),
    ("0.1 - 3.0.0-0", 0x2_7fff_fff8),
    ("1.2.3-alpha - 2.3.4", 0x2_3ffa_0000),
    ("1.2.3-alpha - 2.3", 0x2_3ffa_0000),
    ("1.2.3-alpha - 2", 0x2_7ffa_0000),
    ("1.2.3-alpha - 1.5.0", 0x2_00fa_0000),
    ("1.2.3-alpha - 2.0.0", 0x2_0ffa_0000),
    ("1.2.3-alpha - 2.0.0-rc.1", 0x2_0dfa_0000),
    ("1.2.3-alpha - 1.2.3", 0x2_003a_0000),
    ("1.2.3-alpha - 1.3", 0x2_00fa_0000),
    ("1.2.3-alpha - x", 0x3_fffa_0000),
    ("1.2.3-alpha - *", 0x3_fffa_0000),
    ("1.2.3-alpha - 3.0.0-0", 0x2_7ffa_0000),
    ("1.2.3 - 2.3.4", 0x2_3ffe_0000),
    ("1.2.3 - 2.3", 0x2_3ffe_0000),
    ("1.2.3 - 2", 0x2_7ffe_0000),
    ("1.2.3 - 1.5.0", 0x2_00fe_0000),
    ("1.2.3 - 2.0.0", 0x2_0ffe_0000),
    ("1.2.3 - 2.0.0-rc.1", 0x2_0dfe_0000),
    ("1.2.3 - 1.2.3", 0x2_003e_0000),
    ("1.2.3 - 1.3", 0x2_00fe_0000),
    ("1.2.3 - x", 0x3_fffe_0000),
    ("1.2.3 - *", 0x3_fffe_0000),
    ("1.2.3 - 3.0.0-0", 0x2_7ffe_0000),
    ("x - 2.3.4", 0x6_3fff_ffff),
    ("x - 2.3", 0x6_3fff_ffff),
    ("x - 2", 0x6_7fff_ffff),
    ("x - 1.5.0", 0x6_00ff_ffff),
    ("x - 2.0.0", 0x6_0fff_ffff),
    ("x - 2.0.0-rc.1", 0x6_0dff_ffff),
    ("x - 1.2.3", 0x6_003f_ffff),
    ("x - 1.3", 0x6_00ff_ffff),
    ("x - x", 0x7_ffff_ffff),
    ("x - *", 0x7_ffff_ffff),
    ("x - 3.0.0-0", 0x6_7fff_ffff),
    ("* - 2.3.4", 0x6_3fff_ffff),
    ("* - 2.3", 0x6_3fff_ffff),
    ("* - 2", 0x6_7fff_ffff),
    ("* - 1.5.0", 0x6_00ff_ffff),
    ("* - 2.0.0", 0x6_0fff_ffff),
    ("* - 2.0.0-rc.1", 0x6_0dff_ffff),
    ("* - 1.2.3", 0x6_003f_ffff),
    ("* - 1.3", 0x6_00ff_ffff),
    ("* - x", 0x7_ffff_ffff),
    ("* - *", 0x7_ffff_ffff),
    ("* - 3.0.0-0", 0x6_7fff_ffff),
    ("0 - 2.3.4", 0x6_3fff_ffff),
    ("0 - 2.3", 0x6_3fff_ffff),
    ("0 - 2", 0x6_7fff_ffff),
    ("0 - 1.5.0", 0x6_00ff_ffff),
    ("0 - 2.0.0", 0x6_0fff_ffff),
    ("0 - 2.0.0-rc.1", 0x6_0dff_ffff),
    ("0 - 1.2.3", 0x6_003f_ffff),
    ("0 - 1.3", 0x6_00ff_ffff),
    ("0 - x", 0x7_ffff_ffff),
    ("0 - *", 0x7_ffff_ffff),
    ("0 - 3.0.0-0", 0x6_7fff_ffff),
    ("1 - 2.3.4", 0x2_3fff_ff80),
    ("1 - 2.3", 0x2_3fff_ff80),
    ("1 - 2", 0x2_7fff_ff80),
    ("1 - 1.5.0", 0x2_00ff_ff80),
    ("1 - 2.0.0", 0x2_0fff_ff80),
    ("1 - 2.0.0-rc.1", 0x2_0dff_ff80),
    ("1 - 1.2.3", 0x2_003f_ff80),
    ("1 - 1.3", 0x2_00ff_ff80),
    ("1 - x", 0x3_ffff_ff80),
    ("1 - *", 0x3_ffff_ff80),
    ("1 - 3.0.0-0", 0x2_7fff_ff80),
    (">=1.2.3 || ^2.0.0", 0x3_ffc2_0000),
    (">=1.2.3  ||  ^2.0.0", 0x3_ffc2_0000),
    (">=1.2.3 || 2.0.0", 0x3_ffc2_0000),
    (">=1.2.3  ||  2.0.0", 0x3_ffc2_0000),
    (">=1.2.3 || <3.0.0", 0x7_ffff_ffff),
    (">=1.2.3  ||  <3.0.0", 0x7_ffff_ffff),
    (">=1.2.3 || >=2.0.0", 0x3_ffc2_0000),
    (">=1.2.3  ||  >=2.0.0", 0x3_ffc2_0000),
    (">=1.2.3 || ~2.1", 0x3_ffc2_0000),
    (">=1.2.3  ||  ~2.1", 0x3_ffc2_0000),
    (">=1.2.3 || 1.x", 0x3_ffff_ff80),
    (">=1.2.3  ||  1.x", 0x3_ffff_ff80),
    (">=1.2.3 || *", 0x7_ffff_ffff),
    (">=1.2.3  ||  *", 0x7_ffff_ffff),
    (">=1.2.3 || >=0.0.0-0", 0x7_ffff_ffff),
    (">=1.2.3  ||  >=0.0.0-0", 0x7_ffff_ffff),
    (">1.2.3 || ^2.0.0", 0x1_ffc0_0000),
    (">1.2.3  ||  ^2.0.0", 0x1_ffc0_0000),
    (">1.2.3 || 2.0.0", 0x1_ffc0_0000),
    (">1.2.3  ||  2.0.0", 0x1_ffc0_0000),
    (">1.2.3 || <3.0.0", 0x7_ffff_ffff),
    (">1.2.3  ||  <3.0.0", 0x7_ffff_ffff),
    (">1.2.3 || >=2.0.0", 0x1_ffc0_0000),
    (">1.2.3  ||  >=2.0.0", 0x1_ffc0_0000),
    (">1.2.3 || ~2.1", 0x1_ffc0_0000),
    (">1.2.3  ||  ~2.1", 0x1_ffc0_0000),
    (">1.2.3 || 1.x", 0x3_ffff_ff80),
    (">1.2.3  ||  1.x", 0x3_ffff_ff80),
    (">1.2.3 || *", 0x7_ffff_ffff),
    (">1.2.3  ||  *", 0x7_ffff_ffff),
    (">1.2.3 || >=0.0.0-0", 0x7_ffff_ffff),
    (">1.2.3  ||  >=0.0.0-0", 0x7_ffff_ffff),
    ("<2.0.0", 0x6_0dff_ffff),
    ("<2.0.0 || ^2.0.0", 0x6_7fff_ffff),
    ("<2.0.0  ||  ^2.0.0", 0x6_7fff_ffff),
    ("<2.0.0 || 2.0.0", 0x6_0fff_ffff),
    ("<2.0.0  ||  2.0.0", 0x6_0fff_ffff),
    ("<2.0.0 || <3.0.0", 0x6_7fff_ffff),
    ("<2.0.0  ||  <3.0.0", 0x6_7fff_ffff),
    ("<2.0.0 || >=2.0.0", 0x7_ffff_ffff),
    ("<2.0.0  ||  >=2.0.0", 0x7_ffff_ffff),
    ("<2.0.0 || ~2.1", 0x6_2dff_ffff),
    ("<2.0.0  ||  ~2.1", 0x6_2dff_ffff),
    ("<2.0.0 || 1.x", 0x6_0dff_ffff),
    ("<2.0.0  ||  1.x", 0x6_0dff_ffff),
    ("<2.0.0 || *", 0x7_ffff_ffff),
    ("<2.0.0  ||  *", 0x7_ffff_ffff),
    ("<2.0.0 || >=0.0.0-0", 0x7_ffff_ffff),
    ("<2.0.0  ||  >=0.0.0-0", 0x7_ffff_ffff),
    ("<=2.0.0", 0x6_0fff_ffff),
    ("<=2.0.0 || ^2.0.0", 0x6_7fff_ffff),
    ("<=2.0.0  ||  ^2.0.0", 0x6_7fff_ffff),
    ("<=2.0.0 || 2.0.0", 0x6_0fff_ffff),
    ("<=2.0.0  ||  2.0.0", 0x6_0fff_ffff),
    ("<=2.0.0 || <3.0.0", 0x6_7fff_ffff),
    ("<=2.0.0  ||  <3.0.0", 0x6_7fff_ffff),
    ("<=2.0.0 || >=2.0.0", 0x7_ffff_ffff),
    ("<=2.0.0  ||  >=2.0.0", 0x7_ffff_ffff),
    ("<=2.0.0 || ~2.1", 0x6_2fff_ffff),
    ("<=2.0.0  ||  ~2.1", 0x6_2fff_ffff),
    ("<=2.0.0 || 1.x", 0x6_0fff_ffff),
    ("<=2.0.0  ||  1.x", 0x6_0fff_ffff),
    ("<=2.0.0 || *", 0x7_ffff_ffff),
    ("<=2.0.0  ||  *", 0x7_ffff_ffff),
    ("<=2.0.0 || >=0.0.0-0", 0x7_ffff_ffff),
    ("<=2.0.0  ||  >=0.0.0-0", 0x7_ffff_ffff),
    ("<2.0.0-0 || ^2.0.0", 0x6_73ff_ffff),
    ("<2.0.0-0  ||  ^2.0.0", 0x6_73ff_ffff),
    ("<2.0.0-0 || 2.0.0", 0x6_03ff_ffff),
    ("<2.0.0-0  ||  2.0.0", 0x6_03ff_ffff),
    ("<2.0.0-0 || <3.0.0", 0x6_7fff_ffff),
    ("<2.0.0-0  ||  <3.0.0", 0x6_7fff_ffff),
    ("<2.0.0-0 || >=2.0.0", 0x7_f3ff_ffff),
    ("<2.0.0-0  ||  >=2.0.0", 0x7_f3ff_ffff),
    ("<2.0.0-0 || ~2.1", 0x6_21ff_ffff),
    ("<2.0.0-0  ||  ~2.1", 0x6_21ff_ffff),
    ("<2.0.0-0 || 1.x", 0x6_01ff_ffff),
    ("<2.0.0-0  ||  1.x", 0x6_01ff_ffff),
    ("<2.0.0-0 || *", 0x7_ffff_ffff),
    ("<2.0.0-0  ||  *", 0x7_ffff_ffff),
    ("<2.0.0-0 || >=0.0.0-0", 0x7_ffff_ffff),
    ("<2.0.0-0  ||  >=0.0.0-0", 0x7_ffff_ffff),
    (">=1.0.0-0", 0x3_ffff_ff80),
    (">=1.0.0-0 || ^2.0.0", 0x3_ffff_ff80),
    (">=1.0.0-0  ||  ^2.0.0", 0x3_ffff_ff80),
    (">=1.0.0-0 || 2.0.0", 0x3_ffff_ff80),
    (">=1.0.0-0  ||  2.0.0", 0x3_ffff_ff80),
    (">=1.0.0-0 || <3.0.0", 0x7_ffff_ffff),
    (">=1.0.0-0  ||  <3.0.0", 0x7_ffff_ffff),
    (">=1.0.0-0 || >=2.0.0", 0x3_ffff_ff80),
    (">=1.0.0-0  ||  >=2.0.0", 0x3_ffff_ff80),
    (">=1.0.0-0 || ~2.1", 0x3_ffff_ff80),
    (">=1.0.0-0  ||  ~2.1", 0x3_ffff_ff80),
    (">=1.0.0-0 || 1.x", 0x3_ffff_ff80),
    (">=1.0.0-0  ||  1.x", 0x3_ffff_ff80),
    (">=1.0.0-0 || *", 0x7_ffff_ffff),
    (">=1.0.0-0  ||  *", 0x7_ffff_ffff),
    (">=1.0.0-0 || >=0.0.0-0", 0x7_ffff_ffff),
    (">=1.0.0-0  ||  >=0.0.0-0", 0x7_ffff_ffff),
    ("^1.2 || ^2.0.0", 0x2_73ff_8000),
    ("^1.2  ||  ^2.0.0", 0x2_73ff_8000),
    ("^1.2 || 2.0.0", 0x2_03ff_8000),
    ("^1.2  ||  2.0.0", 0x2_03ff_8000),
    ("^1.2 || <3.0.0", 0x6_7fff_ffff),
    ("^1.2  ||  <3.0.0", 0x6_7fff_ffff),
    ("^1.2 || >=2.0.0", 0x3_f3ff_8000),
    ("^1.2  ||  >=2.0.0", 0x3_f3ff_8000),
    ("^1.2 || ~2.1", 0x2_21ff_8000),
    ("^1.2  ||  ~2.1", 0x2_21ff_8000),
    ("^1.2 || 1.x", 0x2_01ff_ff80),
    ("^1.2  ||  1.x", 0x2_01ff_ff80),
    ("^1.2 || *", 0x7_ffff_ffff),
    ("^1.2  ||  *", 0x7_ffff_ffff),
    ("^1.2 || >=0.0.0-0", 0x7_ffff_ffff),
    ("^1.2  ||  >=0.0.0-0", 0x7_ffff_ffff),
    ("~1.2 || ^2.0.0", 0x2_727f_8000),
    ("~1.2  ||  ^2.0.0", 0x2_727f_8000),
    ("~1.2 || 2.0.0", 0x2_027f_8000),
    ("~1.2  ||  2.0.0", 0x2_027f_8000),
    ("~1.2 || <3.0.0", 0x6_7fff_ffff),
    ("~1.2  ||  <3.0.0", 0x6_7fff_ffff),
    ("~1.2 || >=2.0.0", 0x3_f27f_8000),
    ("~1.2  ||  >=2.0.0", 0x3_f27f_8000),
    ("~1.2 || ~2.1", 0x2_207f_8000),
    ("~1.2  ||  ~2.1", 0x2_207f_8000),
    ("~1.2 || 1.x", 0x2_01ff_ff80),
    ("~1.2  ||  1.x", 0x2_01ff_ff80),
    ("~1.2 || *", 0x7_ffff_ffff),
    ("~1.2  ||  *", 0x7_ffff_ffff),
    ("~1.2 || >=0.0.0-0", 0x7_ffff_ffff),
    ("~1.2  ||  >=0.0.0-0", 0x7_ffff_ffff),
    ("1.2.x || ^2.0.0", 0x2_727f_8000),
    ("1.2.x  ||  ^2.0.0", 0x2_727f_8000),
    ("1.2.x || 2.0.0", 0x2_027f_8000),
    ("1.2.x  ||  2.0.0", 0x2_027f_8000),
    ("1.2.x || <3.0.0", 0x6_7fff_ffff),
    ("1.2.x  ||  <3.0.0", 0x6_7fff_ffff),
    ("1.2.x || >=2.0.0", 0x3_f27f_8000),
    ("1.2.x  ||  >=2.0.0", 0x3_f27f_8000),
    ("1.2.x || ~2.1", 0x2_207f_8000),
    ("1.2.x  ||  ~2.1", 0x2_207f_8000),
    ("1.2.x || 1.x", 0x2_01ff_ff80),
    ("1.2.x  ||  1.x", 0x2_01ff_ff80),
    ("1.2.x || *", 0x7_ffff_ffff),
    ("1.2.x  ||  *", 0x7_ffff_ffff),
    ("1.2.x || >=0.0.0-0", 0x7_ffff_ffff),
    ("1.2.x  ||  >=0.0.0-0", 0x7_ffff_ffff),
    (">=1.2.3 <2.0.0", 0x2_0dc2_0000),
    (">=1.2.3 <2.0.0 || ^2.0.0", 0x2_7fc2_0000),
    (">=1.2.3 <2.0.0  ||  ^2.0.0", 0x2_7fc2_0000),
    (">=1.2.3 <2.0.0 || 2.0.0", 0x2_0fc2_0000),
    (">=1.2.3 <2.0.0  ||  2.0.0", 0x2_0fc2_0000),
    (">=1.2.3 <2.0.0 || <3.0.0", 0x6_7fff_ffff),
    (">=1.2.3 <2.0.0  ||  <3.0.0", 0x6_7fff_ffff),
    (">=1.2.3 <2.0.0 || >=2.0.0", 0x3_ffc2_0000),
    (">=1.2.3 <2.0.0  ||  >=2.0.0", 0x3_ffc2_0000),
    (">=1.2.3 <2.0.0 || ~2.1", 0x2_2dc2_0000),
    (">=1.2.3 <2.0.0  ||  ~2.1", 0x2_2dc2_0000),
    (">=1.2.3 <2.0.0 || 1.x", 0x2_0dff_ff80),
    (">=1.2.3 <2.0.0  ||  1.x", 0x2_0dff_ff80),
    (">=1.2.3 <2.0.0 || *", 0x7_ffff_ffff),
    (">=1.2.3 <2.0.0  ||  *", 0x7_ffff_ffff),
    (">=1.2.3 <2.0.0 || >=0.0.0-0", 0x7_ffff_ffff),
    (">=1.2.3 <2.0.0  ||  >=0.0.0-0", 0x7_ffff_ffff),
    ("> 1.2.3 < 2.0.0", 0xdc0_0000),
    ("> 1.2.3 < 2.0.0 || ^2.0.0", 0x7fc0_0000),
    ("> 1.2.3 < 2.0.0  ||  ^2.0.0", 0x7fc0_0000),
    ("> 1.2.3 < 2.0.0 || 2.0.0", 0xfc0_0000),
    ("> 1.2.3 < 2.0.0  ||  2.0.0", 0xfc0_0000),
    ("> 1.2.3 < 2.0.0 || <3.0.0", 0x6_7fff_ffff),
    ("> 1.2.3 < 2.0.0  ||  <3.0.0", 0x6_7fff_ffff),
    ("> 1.2.3 < 2.0.0 || >=2.0.0", 0x1_ffc0_0000),
    ("> 1.2.3 < 2.0.0  ||  >=2.0.0", 0x1_ffc0_0000),
    ("> 1.2.3 < 2.0.0 || ~2.1", 0x2dc0_0000),
    ("> 1.2.3 < 2.0.0  ||  ~2.1", 0x2dc0_0000),
    ("> 1.2.3 < 2.0.0 || 1.x", 0x2_0dff_ff80),
    ("> 1.2.3 < 2.0.0  ||  1.x", 0x2_0dff_ff80),
    ("> 1.2.3 < 2.0.0 || *", 0x7_ffff_ffff),
    ("> 1.2.3 < 2.0.0  ||  *", 0x7_ffff_ffff),
    ("> 1.2.3 < 2.0.0 || >=0.0.0-0", 0x7_ffff_ffff),
    ("> 1.2.3 < 2.0.0  ||  >=0.0.0-0", 0x7_ffff_ffff),
    (">= 1.2.3", 0x3_ffc2_0000),
    (">= 1.2.3 || ^2.0.0", 0x3_ffc2_0000),
    (">= 1.2.3  ||  ^2.0.0", 0x3_ffc2_0000),
    (">= 1.2.3 || 2.0.0", 0x3_ffc2_0000),
    (">= 1.2.3  ||  2.0.0", 0x3_ffc2_0000),
    (">= 1.2.3 || <3.0.0", 0x7_ffff_ffff),
    (">= 1.2.3  ||  <3.0.0", 0x7_ffff_ffff),
    (">= 1.2.3 || >=2.0.0", 0x3_ffc2_0000),
    (">= 1.2.3  ||  >=2.0.0", 0x3_ffc2_0000),
    (">= 1.2.3 || ~2.1", 0x3_ffc2_0000),
    (">= 1.2.3  ||  ~2.1", 0x3_ffc2_0000),
    (">= 1.2.3 || 1.x", 0x3_ffff_ff80),
    (">= 1.2.3  ||  1.x", 0x3_ffff_ff80),
    (">= 1.2.3 || *", 0x7_ffff_ffff),
    (">= 1.2.3  ||  *", 0x7_ffff_ffff),
    (">= 1.2.3 || >=0.0.0-0", 0x7_ffff_ffff),
    (">= 1.2.3  ||  >=0.0.0-0", 0x7_ffff_ffff),
    ("<= 1.2", 0x6_007f_ffff),
    ("<= 1.2 || ^2.0.0", 0x6_727f_ffff),
    ("<= 1.2  ||  ^2.0.0", 0x6_727f_ffff),
    ("<= 1.2 || 2.0.0", 0x6_027f_ffff),
    ("<= 1.2  ||  2.0.0", 0x6_027f_ffff),
    ("<= 1.2 || <3.0.0", 0x6_7fff_ffff),
    ("<= 1.2  ||  <3.0.0", 0x6_7fff_ffff),
    ("<= 1.2 || >=2.0.0", 0x7_f27f_ffff),
    ("<= 1.2  ||  >=2.0.0", 0x7_f27f_ffff),
    ("<= 1.2 || ~2.1", 0x6_207f_ffff),
    ("<= 1.2  ||  ~2.1", 0x6_207f_ffff),
    ("<= 1.2 || 1.x", 0x6_01ff_ffff),
    ("<= 1.2  ||  1.x", 0x6_01ff_ffff),
    ("<= 1.2 || *", 0x7_ffff_ffff),
    ("<= 1.2  ||  *", 0x7_ffff_ffff),
    ("<= 1.2 || >=0.0.0-0", 0x7_ffff_ffff),
    ("<= 1.2  ||  >=0.0.0-0", 0x7_ffff_ffff),
    ("~ 1.2", 0x2_007f_8000),
    ("~ 1.2 || ^2.0.0", 0x2_727f_8000),
    ("~ 1.2  ||  ^2.0.0", 0x2_727f_8000),
    ("~ 1.2 || 2.0.0", 0x2_027f_8000),
    ("~ 1.2  ||  2.0.0", 0x2_027f_8000),
    ("~ 1.2 || <3.0.0", 0x6_7fff_ffff),
    ("~ 1.2  ||  <3.0.0", 0x6_7fff_ffff),
    ("~ 1.2 || >=2.0.0", 0x3_f27f_8000),
    ("~ 1.2  ||  >=2.0.0", 0x3_f27f_8000),
    ("~ 1.2 || ~2.1", 0x2_207f_8000),
    ("~ 1.2  ||  ~2.1", 0x2_207f_8000),
    ("~ 1.2 || 1.x", 0x2_01ff_ff80),
    ("~ 1.2  ||  1.x", 0x2_01ff_ff80),
    ("~ 1.2 || *", 0x7_ffff_ffff),
    ("~ 1.2  ||  *", 0x7_ffff_ffff),
    ("~ 1.2 || >=0.0.0-0", 0x7_ffff_ffff),
    ("~ 1.2  ||  >=0.0.0-0", 0x7_ffff_ffff),
    ("^ 1.2", 0x2_01ff_8000),
    ("^ 1.2 || ^2.0.0", 0x2_73ff_8000),
    ("^ 1.2  ||  ^2.0.0", 0x2_73ff_8000),
    ("^ 1.2 || 2.0.0", 0x2_03ff_8000),
    ("^ 1.2  ||  2.0.0", 0x2_03ff_8000),
    ("^ 1.2 || <3.0.0", 0x6_7fff_ffff),
    ("^ 1.2  ||  <3.0.0", 0x6_7fff_ffff),
    ("^ 1.2 || >=2.0.0", 0x3_f3ff_8000),
    ("^ 1.2  ||  >=2.0.0", 0x3_f3ff_8000),
    ("^ 1.2 || ~2.1", 0x2_21ff_8000),
    ("^ 1.2  ||  ~2.1", 0x2_21ff_8000),
    ("^ 1.2 || 1.x", 0x2_01ff_ff80),
    ("^ 1.2  ||  1.x", 0x2_01ff_ff80),
    ("^ 1.2 || *", 0x7_ffff_ffff),
    ("^ 1.2  ||  *", 0x7_ffff_ffff),
    ("^ 1.2 || >=0.0.0-0", 0x7_ffff_ffff),
    ("^ 1.2  ||  >=0.0.0-0", 0x7_ffff_ffff),
    (">1.2 <1.9", 0x80_0000),
    (">1.2 <1.9 || ^2.0.0", 0x7280_0000),
    (">1.2 <1.9  ||  ^2.0.0", 0x7280_0000),
    (">1.2 <1.9 || 2.0.0", 0x280_0000),
    (">1.2 <1.9  ||  2.0.0", 0x280_0000),
    (">1.2 <1.9 || <3.0.0", 0x6_7fff_ffff),
    (">1.2 <1.9  ||  <3.0.0", 0x6_7fff_ffff),
    (">1.2 <1.9 || >=2.0.0", 0x1_f280_0000),
    (">1.2 <1.9  ||  >=2.0.0", 0x1_f280_0000),
    (">1.2 <1.9 || ~2.1", 0x2080_0000),
    (">1.2 <1.9  ||  ~2.1", 0x2080_0000),
    (">1.2 <1.9 || 1.x", 0x2_01ff_ff80),
    (">1.2 <1.9  ||  1.x", 0x2_01ff_ff80),
    (">1.2 <1.9 || *", 0x7_ffff_ffff),
    (">1.2 <1.9  ||  *", 0x7_ffff_ffff),
    (">1.2 <1.9 || >=0.0.0-0", 0x7_ffff_ffff),
    (">1.2 <1.9  ||  >=0.0.0-0", 0x7_ffff_ffff),
    (">=1.2.3 <1.9.0", 0x2_00c2_0000),
    (">=1.2.3 <1.9.0 || ^2.0.0", 0x2_72c2_0000),
    (">=1.2.3 <1.9.0  ||  ^2.0.0", 0x2_72c2_0000),
    (">=1.2.3 <1.9.0 || 2.0.0", 0x2_02c2_0000),
    (">=1.2.3 <1.9.0  ||  2.0.0", 0x2_02c2_0000),
    (">=1.2.3 <1.9.0 || <3.0.0", 0x6_7fff_ffff),
    (">=1.2.3 <1.9.0  ||  <3.0.0", 0x6_7fff_ffff),
    (">=1.2.3 <1.9.0 || >=2.0.0", 0x3_f2c2_0000),
    (">=1.2.3 <1.9.0  ||  >=2.0.0", 0x3_f2c2_0000),
    (">=1.2.3 <1.9.0 || ~2.1", 0x2_20c2_0000),
    (">=1.2.3 <1.9.0  ||  ~2.1", 0x2_20c2_0000),
    (">=1.2.3 <1.9.0 || 1.x", 0x2_01ff_ff80),
    (">=1.2.3 <1.9.0  ||  1.x", 0x2_01ff_ff80),
    (">=1.2.3 <1.9.0 || *", 0x7_ffff_ffff),
    (">=1.2.3 <1.9.0  ||  *", 0x7_ffff_ffff),
    (">=1.2.3 <1.9.0 || >=0.0.0-0", 0x7_ffff_ffff),
    (">=1.2.3 <1.9.0  ||  >=0.0.0-0", 0x7_ffff_ffff),
    (">=1.2.3 <=1.9.9", 0x2_01c2_0000),
    (">=1.2.3 <=1.9.9 || ^2.0.0", 0x2_73c2_0000),
    (">=1.2.3 <=1.9.9  ||  ^2.0.0", 0x2_73c2_0000),
    (">=1.2.3 <=1.9.9 || 2.0.0", 0x2_03c2_0000),
    (">=1.2.3 <=1.9.9  ||  2.0.0", 0x2_03c2_0000),
    (">=1.2.3 <=1.9.9 || <3.0.0", 0x6_7fff_ffff),
    (">=1.2.3 <=1.9.9  ||  <3.0.0", 0x6_7fff_ffff),
    (">=1.2.3 <=1.9.9 || >=2.0.0", 0x3_f3c2_0000),
    (">=1.2.3 <=1.9.9  ||  >=2.0.0", 0x3_f3c2_0000),
    (">=1.2.3 <=1.9.9 || ~2.1", 0x2_21c2_0000),
    (">=1.2.3 <=1.9.9  ||  ~2.1", 0x2_21c2_0000),
    (">=1.2.3 <=1.9.9 || 1.x", 0x2_01ff_ff80),
    (">=1.2.3 <=1.9.9  ||  1.x", 0x2_01ff_ff80),
    (">=1.2.3 <=1.9.9 || *", 0x7_ffff_ffff),
    (">=1.2.3 <=1.9.9  ||  *", 0x7_ffff_ffff),
    (">=1.2.3 <=1.9.9 || >=0.0.0-0", 0x7_ffff_ffff),
    (">=1.2.3 <=1.9.9  ||  >=0.0.0-0", 0x7_ffff_ffff),
    (">0.0.0 || ^2.0.0", 0x3_ffff_fffe),
    (">0.0.0  ||  ^2.0.0", 0x3_ffff_fffe),
    (">0.0.0 || 2.0.0", 0x3_ffff_fffe),
    (">0.0.0  ||  2.0.0", 0x3_ffff_fffe),
    (">0.0.0 || <3.0.0", 0x7_ffff_ffff),
    (">0.0.0  ||  <3.0.0", 0x7_ffff_ffff),
    (">0.0.0 || >=2.0.0", 0x3_ffff_fffe),
    (">0.0.0  ||  >=2.0.0", 0x3_ffff_fffe),
    (">0.0.0 || ~2.1", 0x3_ffff_fffe),
    (">0.0.0  ||  ~2.1", 0x3_ffff_fffe),
    (">0.0.0 || 1.x", 0x3_ffff_fffe),
    (">0.0.0  ||  1.x", 0x3_ffff_fffe),
    (">0.0.0 || *", 0x7_ffff_ffff),
    (">0.0.0  ||  *", 0x7_ffff_ffff),
    (">0.0.0 || >=0.0.0-0", 0x7_ffff_ffff),
    (">0.0.0  ||  >=0.0.0-0", 0x7_ffff_ffff),
    (">=0.0.0 || ^2.0.0", 0x3_ffff_ffff),
    (">=0.0.0  ||  ^2.0.0", 0x3_ffff_ffff),
    (">=0.0.0 || 2.0.0", 0x3_ffff_ffff),
    (">=0.0.0  ||  2.0.0", 0x3_ffff_ffff),
    (">=0.0.0 || <3.0.0", 0x7_ffff_ffff),
    (">=0.0.0  ||  <3.0.0", 0x7_ffff_ffff),
    (">=0.0.0 || >=2.0.0", 0x3_ffff_ffff),
    (">=0.0.0  ||  >=2.0.0", 0x3_ffff_ffff),
    (">=0.0.0 || ~2.1", 0x3_ffff_ffff),
    (">=0.0.0  ||  ~2.1", 0x3_ffff_ffff),
    (">=0.0.0 || 1.x", 0x3_ffff_ffff),
    (">=0.0.0  ||  1.x", 0x3_ffff_ffff),
    (">=0.0.0 || *", 0x7_ffff_ffff),
    (">=0.0.0  ||  *", 0x7_ffff_ffff),
    (">=0.0.0 || >=0.0.0-0", 0x7_ffff_ffff),
    (">=0.0.0  ||  >=0.0.0-0", 0x7_ffff_ffff),
    (">=0.0.0-0", 0x7_ffff_ffff),
    (">=0.0.0-0 || ^2.0.0", 0x7_ffff_ffff),
    (">=0.0.0-0  ||  ^2.0.0", 0x7_ffff_ffff),
    (">=0.0.0-0 || 2.0.0", 0x7_ffff_ffff),
    (">=0.0.0-0  ||  2.0.0", 0x7_ffff_ffff),
    (">=0.0.0-0 || <3.0.0", 0x7_ffff_ffff),
    (">=0.0.0-0  ||  <3.0.0", 0x7_ffff_ffff),
    (">=0.0.0-0 || >=2.0.0", 0x7_ffff_ffff),
    (">=0.0.0-0  ||  >=2.0.0", 0x7_ffff_ffff),
    (">=0.0.0-0 || ~2.1", 0x7_ffff_ffff),
    (">=0.0.0-0  ||  ~2.1", 0x7_ffff_ffff),
    (">=0.0.0-0 || 1.x", 0x7_ffff_ffff),
    (">=0.0.0-0  ||  1.x", 0x7_ffff_ffff),
    (">=0.0.0-0 || *", 0x7_ffff_ffff),
    (">=0.0.0-0  ||  *", 0x7_ffff_ffff),
    (">=0.0.0-0 || >=0.0.0-0", 0x7_ffff_ffff),
    (">=0.0.0-0  ||  >=0.0.0-0", 0x7_ffff_ffff),
    ("<0.0.0-0", 0x0),
    ("<0.0.0-0 || ^2.0.0", 0x7200_0000),
    ("<0.0.0-0  ||  ^2.0.0", 0x7200_0000),
    ("<0.0.0-0 || 2.0.0", 0x200_0000),
    ("<0.0.0-0  ||  2.0.0", 0x200_0000),
    ("<0.0.0-0 || <3.0.0", 0x6_7fff_ffff),
    ("<0.0.0-0  ||  <3.0.0", 0x6_7fff_ffff),
    ("<0.0.0-0 || >=2.0.0", 0x1_f200_0000),
    ("<0.0.0-0  ||  >=2.0.0", 0x1_f200_0000),
    ("<0.0.0-0 || ~2.1", 0x2000_0000),
    ("<0.0.0-0  ||  ~2.1", 0x2000_0000),
    ("<0.0.0-0 || 1.x", 0x2_01ff_ff80),
    ("<0.0.0-0  ||  1.x", 0x2_01ff_ff80),
    ("<0.0.0-0 || *", 0x7_ffff_ffff),
    ("<0.0.0-0  ||  *", 0x7_ffff_ffff),
    ("<0.0.0-0 || >=0.0.0-0", 0x7_ffff_ffff),
    ("<0.0.0-0  ||  >=0.0.0-0", 0x7_ffff_ffff),
    ("", 0x7_ffff_ffff),
    ("  ", 0x7_ffff_ffff),
    ("not a range", 0x0),
    ("blah", 0x0),
    ("^", 0x0),
    (">=", 0x0),
    (">", 0x0),
    ("<", 0x0),
    ("~", 0x0),
    ("^1.2.3.4", 0x0),
    ("1.2.3.4", 0x0),
    (">=<1.0.0", 0x0),
    ("x.1", 0x0),
    ("1.x.3", 0x0),
    ("||", 0x7_ffff_ffff),
    ("1.2.3 ||", 0x7_ffff_ffff),
    ("|| 1.2.3", 0x7_ffff_ffff),
    ("|||", 0x0),
    ("1.2.3|||2.0.0", 0x0),
    ("=", 0x0),
    ("==", 0x0),
    (">=  ", 0x0),
    ("1.2.3 -", 0x0),
    ("- 1.2.3", 0x0),
    ("1.2.3 - - 2.0.0", 0x0),
    ("1.2.3- - 2.0.0", 0x0),
    ("01.2.3", 0x0),
    ("1.02.3", 0x0),
    ("1.2.03", 0x0),
    ("V1.2.3", 0x0),
    ("1.2.3-01", 0x0),
    ("1.2.3-alpha..1", 0x0),
    ("1.2.3+", 0x0),
    ("1.2.3+ok.", 0x0),
    ("\t1.2.3\n", 0x2_0002_0000),
    ("^v1.2.3", 0x2_01c2_0000),
    ("~>v1.2", 0x2_007f_8000),
    (">=v1.2.3", 0x3_ffc2_0000),
    ("*x", 0x0),
    ("x*", 0x0),
];

/// The versions every range in [`RANGE_TABLE`] is tried against.
const SATISFYING_VERSIONS: [&str; 35] = [
    "0.0.0",
    "0.0.1",
    "0.0.2",
    "0.1.0",
    "0.1.5",
    "0.2.0",
    "0.2.3",
    "1.0.0",
    "1.0.0-0",
    "1.0.0-1",
    "1.0.0-alpha",
    "1.0.0-alpha.1",
    "1.0.0-beta",
    "1.0.0-rc.1",
    "1.1.0",
    "1.2.0",
    "1.2.2",
    "1.2.3",
    "1.2.3-0",
    "1.2.3-alpha",
    "1.2.3-alpha.1",
    "1.2.3-beta.2",
    "1.2.4",
    "1.3.0",
    "1.9.9",
    "2.0.0",
    "2.0.0-0",
    "2.0.0-rc.1",
    "2.0.1",
    "2.1.0",
    "2.5.0",
    "3.0.0",
    "10.20.30",
    "1.2.3+build.5",
    "0.0.0-0",
];

// ============================================================================================
// The range evaluator — differential against node-semver
// ============================================================================================

/// Every recorded range decision, against the real `node-semver`.
#[test]
fn satisfies_matches_node_semver_for_every_recorded_range() {
    let mut failures = Vec::new();
    for (range, mask) in RANGE_TABLE {
        for (index, candidate) in SATISFYING_VERSIONS.iter().enumerate() {
            let expected = mask & (1 << index) != 0;
            let actual = satisfies(candidate, range);
            if expected != actual {
                failures.push(format!(
                    "{candidate} in {range:?}: node-semver says {expected}, this build says {actual}"
                ));
            }
        }
    }
    assert!(
        failures.is_empty(),
        "{} of {} recorded range decisions disagree:\n{}",
        failures.len(),
        RANGE_TABLE.len() * SATISFYING_VERSIONS.len(),
        failures.join("\n")
    );
}

/// The grammar itself, stated as the rules a reader would check by hand.
#[test]
fn ranges_follow_the_grammar_a_declaration_expects() {
    // An `engines.dsh` range is a range, not a version: a bare number is exactly that version.
    assert!(satisfies("1.2.3", "1.2.3"));
    assert!(!satisfies("1.2.4", "1.2.3"));
    // A partial version is a range over the whole tuple, so `1.2` admits `1.2.9`.
    assert!(satisfies("1.2.9", "1.2"));
    assert!(!satisfies("1.3.0", "1.2"));
    // Caret and tilde differ exactly where backwards compatibility ends.
    assert!(satisfies("1.9.0", "^1.2.3"));
    assert!(!satisfies("2.0.0", "^1.2.3"));
    assert!(satisfies("1.2.9", "~1.2.3"));
    assert!(!satisfies("1.3.0", "~1.2.3"));
    // `^0.x` is the range that bites real plugins: only the last non-zero component may move.
    assert!(satisfies("0.2.9", "^0.2.3"));
    assert!(!satisfies("0.3.0", "^0.2.3"));
    assert!(!satisfies("0.0.2", "^0.0.1"));
    // A hyphen range is inclusive at both ends, and `-0` keeps its upper bound open on prereleases.
    assert!(satisfies("1.2.3", "1.2.3 - 2.3.4"));
    assert!(satisfies("2.3.4", "1.2.3 - 2.3.4"));
    assert!(!satisfies("2.3.5", "1.2.3 - 2.3.4"));
    // Alternatives are OR, comparators within one alternative are AND.
    assert!(satisfies("2.5.0", "^1.2.3 || ^2.0.0"));
    assert!(!satisfies("3.0.0", "^1.2.3 || ^2.0.0"));
    assert!(!satisfies("2.5.0", ">=1.0.0 <2.0.0"));
    // `*` and the empty range admit everything; whitespace between an operator and its version
    // is not a syntax error.
    assert!(satisfies("1.0.0", "*"));
    assert!(satisfies("1.0.0", ""));
    assert!(satisfies("1.0.0", ">= 0.9.0"));
    assert!(satisfies("0.9.5", "~ 0.9"));
    assert!(!satisfies("1.0.0", "~ 0.9"));
    // A malformed range is a refusal, never a permissive `true`.
    assert!(!satisfies("1.0.0", "not a range"));
    assert!(!satisfies("1.0.0", "x.1"));
    assert!(!satisfies("1.0.0", "^"));
}

/// `includePrerelease` is the difference between refusing every DSH release and accepting them.
#[test]
fn prereleases_participate_in_ranges() {
    // The running DSH is a prerelease during its own release window.
    assert!(satisfies("2.0.0-rc.1", "^2.0.0-rc.1"));
    assert!(satisfies("2.0.0-rc.2", "^2.0.0-rc.1"));
    // A bare `^2.0.0` desugars to `>=2.0.0 <3.0.0-0` — note the *absent* `-0` on the lower bound —
    // so it still refuses a prerelease of the very version it starts at.
    assert!(satisfies("2.0.0", "^2.0.0"));
    assert!(!satisfies("2.0.0-rc.1", "^2.0.0"));
    assert!(!satisfies("2.0.0-0", "^2.0.0"));
    // And a range that names a prerelease does not admit an earlier one.
    assert!(!satisfies("2.0.0-rc.1", ">=2.0.0"));
}

// ============================================================================================
// The projections — differential against upstream
// ============================================================================================

/// One recorded outcome, split into its three fields.
fn outcome(field: &str) -> (Option<String>, MarketInstallation) {
    let mut parts = field.split('|');
    let default = parts.next().expect("a default version field");
    let status = parts.next().expect("a status field");
    let installed = parts.next().expect("an installed version field");
    let status = match status {
        "not-installed" => MarketInstallationStatus::NotInstalled,
        "installed" => MarketInstallationStatus::Installed,
        "update-available" => MarketInstallationStatus::UpdateAvailable,
        "different-version" => MarketInstallationStatus::DifferentVersion,
        "provided" => MarketInstallationStatus::Provided,
        other => panic!("unknown recorded status {other:?}"),
    };
    (
        (default != "-").then(|| default.to_owned()),
        MarketInstallation {
            status,
            installed_version: (installed != "-").then(|| installed.to_owned()),
        },
    )
}

/// Every recorded projection, against upstream's own output.
#[test]
#[allow(
    clippy::too_many_lines,
    reason = "one replay loop over the recorded table: splitting it would compute the listing twice and let the compared fields drift apart"
)]
fn listing_matches_upstream_for_every_recorded_projection() {
    let exemptions = exemption_maps();
    let bundles = bundles();
    let mut checked = 0_usize;
    let mut failures = Vec::new();
    for row in PROJECTIONS {
        let fields: Vec<&str> = row.split('|').collect();
        let name = fields[0];
        let bundle_index = fields[1].parse::<usize>().expect("a bundle index");
        let exemption_index = fields[2].parse::<usize>().expect("an exemption index");
        let version_set = fields[3].parse::<usize>().expect("a version set index");
        let compatibility: Vec<&str> = fields[4].split(',').collect();
        let exempted: Vec<&str> = fields[5].split(',').collect();
        let update: Vec<&str> = fields[6].split(',').collect();
        // An outcome contains `|` of its own, so the rest of the row is re-joined first.
        let outcome_field = fields[7..].join("|");
        let outcomes: Vec<&str> = outcome_field.split(',').collect();
        let plugin = fixture(name);
        let expected_versions: Vec<(String, String, Option<String>)> = VERSION_SETS[version_set]
            .iter()
            .map(|(version, spec, registry)| {
                (
                    (*version).to_owned(),
                    (*spec).to_owned(),
                    registry.map(str::to_owned),
                )
            })
            .collect();
        for (runtime_index, runtime) in RUNTIMES.iter().enumerate() {
            let listing = listing_of(
                &plugin,
                &bundles[bundle_index],
                &exemptions[exemption_index],
                runtime,
            );
            let listed: Vec<(String, String, Option<String>)> = listing
                .versions
                .iter()
                .map(|version| {
                    (
                        version.version.version.clone(),
                        version.install.spec.clone(),
                        version.install.registry.clone(),
                    )
                })
                .collect();
            if listed != expected_versions {
                failures.push(format!(
                    "{name} bundle {bundle_index}: install targets {listed:?} != {expected_versions:?}"
                ));
            }
            let compatible_mask =
                u32::from_str_radix(compatibility[runtime_index], 16).expect("a hexadecimal mask");
            let exempted_mask =
                u32::from_str_radix(exempted[runtime_index], 16).expect("a hexadecimal mask");
            let update_mask =
                u32::from_str_radix(update[runtime_index], 16).expect("a hexadecimal mask");
            for (index, listed_version) in listing.versions.iter().enumerate() {
                checked += 1;
                let want = compatible_mask & (1 << index) != 0;
                if listed_version.compatibility.is_compatible() != want {
                    failures.push(format!(
                        "{name} {} on {runtime}: compatible {} != {want}",
                        listed_version.version.version,
                        listed_version.compatibility.is_compatible(),
                    ));
                }
                let want_exempted = exempted_mask & (1 << index) != 0;
                let reported_exempted = match &listed_version.compatibility {
                    MarketCompatibility::Incompatible(issue) => issue.exempted,
                    MarketCompatibility::Compatible => false,
                };
                if reported_exempted != want_exempted {
                    failures.push(format!(
                        "{name} {} on {runtime}: exempted {reported_exempted} != {want_exempted}",
                        listed_version.version.version,
                    ));
                }
                let want = update_mask & (1 << index) != 0;
                if listed_version.update != want {
                    failures.push(format!(
                        "{name} {} on {runtime}: update {} != {want}",
                        listed_version.version.version, listed_version.update,
                    ));
                }
            }
            let (want_default, want_installation) = outcome(outcomes[runtime_index]);
            if listing.default_version != want_default {
                failures.push(format!(
                    "{name} bundle {bundle_index} on {runtime}: default {:?} != {want_default:?}",
                    listing.default_version,
                ));
            }
            if listing.installation != want_installation {
                failures.push(format!(
                    "{name} bundle {bundle_index} on {runtime}: installation {:?} != {want_installation:?}",
                    listing.installation,
                ));
            }
        }
    }
    assert!(
        failures.is_empty(),
        "{} recorded projection decisions disagree:\n{}",
        failures.len(),
        failures.join("\n")
    );
    assert!(checked > 0, "the projection table held no versions");
}

// ============================================================================================
// Compatibility — the rules behind the recorded verdicts
// ============================================================================================

#[test]
fn a_version_without_requirements_is_compatible() {
    let version = npm("1.0.0");
    assert_eq!(
        compatibility_of("plain", &version, &BTreeMap::new(), "2.1.0"),
        MarketCompatibility::Compatible,
    );
}

#[test]
fn an_unmet_range_names_the_declaration_and_the_running_version() {
    let version = version(
        "1.0.0",
        MarketSource::Npm { registry: None },
        engines("^1.0.0"),
    );
    let MarketCompatibility::Incompatible(issue) =
        compatibility_of("plain", &version, &BTreeMap::new(), "2.1.0")
    else {
        panic!("a `^1.0.0` requirement must not be met by 2.1.0");
    };
    assert_eq!(issue.runtime_version, "2.1.0");
    assert_eq!(issue.requirements.len(), 1);
    assert_eq!(issue.requirements[ENGINES_DSH], "^1.0.0");
    assert!(!issue.exempted);
}

/// A blank `engines.dsh` is a broken declaration, not a permissive one.
#[test]
fn a_blank_requirement_is_unmet_rather_than_ignored() {
    for declared in ["", "   "] {
        let version = version(
            "1.0.0",
            MarketSource::Npm { registry: None },
            engines(declared),
        );
        let MarketCompatibility::Incompatible(issue) =
            compatibility_of("plain", &version, &BTreeMap::new(), "2.1.0")
        else {
            panic!("a blank requirement must be reported as unmet");
        };
        assert_eq!(issue.requirements[ENGINES_DSH], declared);
    }
}

#[test]
fn an_exemption_records_consent_without_claiming_the_range_was_met() {
    let version = version(
        "1.0.0",
        MarketSource::Npm { registry: None },
        engines("0.0.1"),
    );
    let granted = BTreeMap::from([("plain@1.0.0".to_owned(), vec!["2.1.0".to_owned()])]);
    let MarketCompatibility::Incompatible(issue) =
        compatibility_of("plain", &version, &granted, "2.1.0")
    else {
        panic!("an exemption must not turn an unmet range into a met one");
    };
    assert!(issue.exempted, "the profile granted this exact pair");
    assert_eq!(issue.requirements[ENGINES_DSH], "0.0.1");
}

/// An exemption names one *exact* pair, so neither a different runtime nor a different version
/// inherits it.
#[test]
fn an_exemption_covers_only_the_exact_version_and_runtime() {
    let version = version(
        "1.0.0",
        MarketSource::Npm { registry: None },
        engines("0.0.1"),
    );
    let cases = [
        // A grant for another runtime.
        BTreeMap::from([("plain@1.0.0".to_owned(), vec!["9.9.9".to_owned()])]),
        // A grant for another version of the same package.
        BTreeMap::from([("plain@2.0.0".to_owned(), vec!["2.1.0".to_owned()])]),
        // A grant that names no runtime at all.
        BTreeMap::from([("plain@1.0.0".to_owned(), Vec::new())]),
    ];
    for exemptions in cases {
        let MarketCompatibility::Incompatible(issue) =
            compatibility_of("plain", &version, &exemptions, "2.1.0")
        else {
            panic!("the range is unmet regardless of the exemption");
        };
        assert!(!issue.exempted, "exemptions are exact: {exemptions:?}");
    }
}

// ============================================================================================
// Installation — the rules behind the recorded statuses
// ============================================================================================

#[test]
fn a_package_the_profile_does_not_list_is_not_installed() {
    assert_eq!(
        installation_of(None, "1.0.0"),
        MarketInstallation {
            status: MarketInstallationStatus::NotInstalled,
            installed_version: None,
        },
    );
}

/// "Provided" means the DSH installation ships the package; an uninstalled profile entry that is
/// merely *discoverable* is not provided.
#[test]
fn only_a_shipped_bundle_is_provided() {
    let shipped = BundleFacts {
        installed: false,
        availability: BundleAvailability::Installation,
        version: Some("1.0.0".to_owned()),
        ..bundle("plain")
    };
    assert_eq!(
        installation_of(Some(&shipped), "2.0.0"),
        MarketInstallation {
            status: MarketInstallationStatus::Provided,
            installed_version: Some("1.0.0".to_owned()),
        },
    );
    for availability in [BundleAvailability::Profile, BundleAvailability::Missing] {
        let listed = BundleFacts {
            installed: false,
            availability,
            ..bundle("plain")
        };
        assert_eq!(
            installation_of(Some(&listed), "2.0.0").status,
            MarketInstallationStatus::NotInstalled,
            "{availability:?} is not a shipped bundle",
        );
    }
}

#[test]
fn an_installed_bundle_is_measured_against_the_listed_version() {
    let measured = "1.0.0";
    let cases = [
        (None, MarketInstallationStatus::DifferentVersion, None),
        (
            Some("1.0.0"),
            MarketInstallationStatus::Installed,
            Some("1.0.0"),
        ),
        (
            Some("0.9.0"),
            MarketInstallationStatus::UpdateAvailable,
            Some("0.9.0"),
        ),
        (
            Some("2.0.0"),
            MarketInstallationStatus::DifferentVersion,
            Some("2.0.0"),
        ),
    ];
    for (installed, status, reported) in cases {
        let facts = BundleFacts {
            version: installed.map(str::to_owned),
            ..bundle("plain")
        };
        assert_eq!(
            installation_of(Some(&facts), measured),
            MarketInstallation {
                status,
                installed_version: reported.map(str::to_owned),
            },
            "installed {installed:?} against {measured}",
        );
    }
}

/// A local copy — `link:../x` with a manifest version like `local` — cannot be ordered, so the
/// measured version is merely different rather than newer.
#[test]
fn an_unparsable_installed_version_is_different_not_newer() {
    let facts = BundleFacts {
        version: Some("local".to_owned()),
        ..bundle("plain")
    };
    assert_eq!(
        installation_of(Some(&facts), "1.0.0"),
        MarketInstallation {
            status: MarketInstallationStatus::DifferentVersion,
            installed_version: Some("local".to_owned()),
        },
    );
}

// ============================================================================================
// The listing — the rules behind the recorded entries
// ============================================================================================

/// The default version is the highest *compatible* stable one; only a plugin with no stable
/// version at all may default to a prerelease.
#[test]
fn the_default_version_prefers_a_compatible_stable_release() {
    let exemptions = BTreeMap::new();
    let mixed = fixture("mixed");
    // `2.0.0-rc.1` is compatible and newest, `1.0.0` is compatible and stable, and `1.1.0` is
    // stable but declares an unmet range.
    assert_eq!(
        listing_of(&mixed, &[], &exemptions, "2.1.0").default_version,
        Some("1.0.0".to_owned()),
    );

    let previews = fixture("previews");
    // No stable version is compatible, so the highest compatible prerelease is the default.
    assert_eq!(
        listing_of(&previews, &[], &exemptions, "2.1.0").default_version,
        Some("1.0.0-beta.1".to_owned()),
    );

    let only_preview = fixture("only-preview-stable");
    // A stable version exists, so no prerelease may be the default — and that stable one is
    // incompatible, so there is no default at all.
    assert_eq!(
        listing_of(&only_preview, &[], &exemptions, "2.1.0").default_version,
        None,
    );
}

/// When nothing is compatible the installation is still measured, against the highest listed
/// version, so the page can say what the profile has.
#[test]
fn an_incompatible_plugin_still_reports_its_installation() {
    let plugin = fixture("all-incompatible");
    let facts = BundleFacts {
        version: Some("0.9.0".to_owned()),
        ..bundle("all-incompatible")
    };
    let listing = listing_of(
        &plugin,
        std::slice::from_ref(&facts),
        &BTreeMap::new(),
        "2.1.0",
    );
    assert_eq!(listing.default_version, None);
    assert_eq!(
        listing.installation,
        MarketInstallation {
            status: MarketInstallationStatus::UpdateAvailable,
            installed_version: Some("0.9.0".to_owned()),
        },
    );
}

/// A Git copy updates only within its own repository directory: another directory of the same
/// repository is another plugin, and a registry copy updates nothing here.
#[test]
fn only_the_same_repository_directory_is_an_update() {
    let plugin = fixture("git-versions");
    let pinned = format!(
        "git+ssh://git@git.example/o/plugins.git#{}&path:/packages/p",
        commit("a"),
    );
    let copy = BundleFacts {
        version: Some("1.0.0".to_owned()),
        update: Some(BundleUpdate::Git),
        source: Some(pinned),
        ..bundle("git-versions")
    };
    let listing = listing_of(
        &plugin,
        std::slice::from_ref(&copy),
        &BTreeMap::new(),
        "2.1.0",
    );
    let updates: Vec<(&str, bool)> = listing
        .versions
        .iter()
        .map(|version| (version.version.version.as_str(), version.update))
        .collect();
    assert_eq!(
        updates,
        vec![
            ("1.3.0", false), // another directory (`packages/q`)
            ("1.2.0", true),  // newer commit, same directory
            ("1.1.0", false), // an npm version
            ("1.0.0", false), // the installed commit itself
            ("0.9.0", false), // older
        ],
    );
}

/// A profile that saved a Git dependency by a forge shortcut still names the same repository.
#[test]
fn a_forge_shortcut_names_the_same_repository() {
    let plugin = fixture("hosted");
    let copy = BundleFacts {
        version: Some("1.0.0".to_owned()),
        update: Some(BundleUpdate::Git),
        source: Some(format!("github:o/plugins#{}&path:/p", commit("a"))),
        ..bundle("hosted")
    };
    let listing = listing_of(
        &plugin,
        std::slice::from_ref(&copy),
        &BTreeMap::new(),
        "2.1.0",
    );
    assert!(
        listing.versions[0].update,
        "`github:o/plugins` and `https://github.com/o/plugins` are one repository",
    );
}

/// A listing is a read: a bundle the profile records under another name changes nothing, and the
/// entry keeps the index's own plugin record.
#[test]
fn a_listing_projects_the_plugin_it_was_given() {
    let plugin = fixture("registry");
    let listing = listing_of(
        &plugin,
        &[BundleFacts {
            name: "other".to_owned(),
            version: Some("1.0.0".to_owned()),
            ..bundle("other")
        }],
        &BTreeMap::new(),
        "2.1.0",
    );
    assert_eq!(listing.plugin.name, "registry");
    assert_eq!(
        listing.versions[0].install.registry.as_deref(),
        Some("https://r.example/"),
        "the version keeps the registry it declares",
    );
    assert_eq!(
        listing.installation.status,
        MarketInstallationStatus::NotInstalled,
    );
}


// Path aliases so a file compiled with #[path] still resolves crate::<domain>::<module>.
mod boot {
    pub use super::boot_gap as install;
    pub use super::market_compat_gap as market_compat;
    pub use super::plugin_market_gap as plugin_market;
}
