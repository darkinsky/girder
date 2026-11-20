//! The plugin market's *profile projection*: what one listed version means for the machine it is
//! being looked at from — whether the running DSH accepts it, whether the profile already has it,
//! and which version a plain "install" would pick.
//!
//! # What these three functions are
//!
//! Upstream `packages/boot/plugin-market/src/index.ts` ends with three module-level functions that
//! every listing goes through. None of them performs I/O; the only thing that makes them *look*
//! world-bound is that the service hands them facts it read a moment earlier:
//!
//! | Upstream | Inputs | Output | Pure? |
//! | --- | --- | --- | --- |
//! | `compatibilityOf(name, version, exemptions, runtimeVersion)` | the plugin name, one [`MarketVersion`], the profile's exact-version exemptions, the running DSH version | `MarketCompatibility` — compatible, or the unmet `engines.dsh` range plus whether the profile exempts this exact pair | **yes** |
//! | `installationOf(bundle, version)` | the profile's bundle record for this package (or none), and the version being measured | `MarketInstallation` — `not-installed`, `provided`, `installed`, `update-available`, `different-version`, plus the installed version when known | **yes** |
//! | `listingOf(plugin, bundles, exemptions, runtimeVersion)` | one index plugin, the whole bundle list, the exemptions, the running version | `MarketListingEntry` — every version with its install arguments, compatibility and update flag, the default version, and the installation status | **yes** |
//!
//! The service wrapper (`annotator`) is what reads `pluginManager.listBundles()`,
//! `pluginManager.listVersionExemptions()` and `getDshRuntimeVersion()`; that wrapper is three
//! awaits of I/O and stays out — exactly as upstream's own `MarketSearch` is pure and its `fetch`
//! is not. So every entry point here takes its facts as parameters. A projection that re-read the
//! real profile would answer differently on a different machine and could not be tested at all,
//! and this one decides what a person is told to install.
//!
//! # What `boot_gap` already covers, and where the boundary is
//!
//! `boot_gap.rs` (2,102 lines) is the *admission* family of `packages/boot`: install specs,
//! registry plans, failure classification, exemption-file parsing, host platform admission, and
//! the semver **precedence** ordering ([`crate::boot_gap::SemVer`]) that `isExactPluginVersion` and
//! `newerVersions` need. It owns, and this module reuses rather than repeats:
//!
//! - [`crate::boot_gap::SemVer`] — parse and precedence order. `listingOf` needs `semver.gt`, and
//!   `installationOf` needs `semver.valid` + `semver.gt`; both are that type's comparison.
//! - [`crate::boot_gap::read_profile_compatibility`] — the exemptions map handed to
//!   [`compatibility_of`] is `CompatibilityFile::exemptions`, byte for byte the type this module
//!   takes. Neither module decides what an exemption *is*; `boot_gap` reads and validates the file,
//!   this module only asks whether one entry names the running version.
//! - [`crate::boot_gap::pinned_git_origin`] — the repository identity `listingOf` compares to
//!   decide whether `updateBundle` would accept a version for an installed Git copy.
//! - [`crate::boot_gap::assert_bundle_platform`], [`crate::boot_gap::is_wsl`],
//!   [`crate::boot_gap::classify_install_failure`], [`crate::boot_gap::registry_plan`] — host and
//!   install-time admission. Nothing here touches them: a market listing is read-only, and whether
//!   a bundle may *run* on this host is not what a listing shows.
//! - [`crate::plugin_market_gap::market_install_target`] — the install spec for one version, which
//!   `listingOf` embeds.
//!
//! What `boot_gap` deliberately does **not** have is the semver **range** evaluator: its module
//! documentation says so and gives the reason ("a partial range evaluator reports compatible
//! plugins as incompatible, and because `engines.dsh` decides alone, a wrong answer is not a
//! degraded answer but a refused install"). `compatibilityOf` cannot exist without one, so
//! [`satisfies`] is implemented here — the whole `node-semver` range grammar that
//! `evaluatePluginCompatibility` uses, with `includePrerelease: true`, not a subset. That is the
//! one piece of this file that is not a projection.
//!
//! What is still *not* here, and belongs to `boot_gap` if it is ever ported: the peer-dependency
//! branch of `evaluatePluginCompatibility` (every `@deepseek-ai/dsh*` `peerDependencies` entry must
//! match when there is no `engines.dsh`) and the "manifest is malformed" throws. Neither can arise
//! from a market listing: upstream's `compatibilityOf` *synthesises* the manifest as
//! `{ name, version, engines: { dsh } }` — a market version carries only `engines.dsh`, never
//! peers — and a [`MarketVersion`] has already been validated by `parseMarketIndex`, so its name,
//! version and range are known well-formed. So the decision this module makes is exactly: *is the
//! declared `engines.dsh` range satisfied by the running version, and if not, does the profile
//! exempt this exact pair*.
//!
//! # What is implemented
//!
//! | Upstream symbol | Here |
//! | --- | --- |
//! | `satisfies` (private, `plugin-compatibility.ts`) | [`satisfies`] |
//! | the whole `node-semver` `Range`/`Comparator` grammar | [`satisfies`]'s parser below |
//! | `compatibilityOf` | [`compatibility_of`], [`MarketCompatibility`], [`MarketIncompatibility`] |
//! | `installationOf` | [`installation_of`], [`MarketInstallation`], [`MarketInstallationStatus`], [`BundleFacts`] |
//! | `listingOf` | [`listing_of`], [`MarketListingEntry`], [`MarketListingVersion`] |
//!
//! # Why the crate root is spelled `girder_core`
//!
//! This module is not declared in `lib.rs` yet — wiring is a separate change. The test compiles
//! this exact source by `#[path]` alongside `boot_gap.rs` and `plugin_market_gap.rs`, so under that
//! inclusion `crate::` names the test binary, whose root declares those same two modules. The two
//! `use crate::…` lines below therefore resolve identically inside the library (where `boot_gap` is
//! a private module of the same crate) and inside the test.

//!
//! # Status: ported, not registered — upstream does not serve it either
//!
//! `compatibility_of`, `installation_of` and `listing_of` are real and complete, and **no RPC endpoint
//! calls them upstream either.** `pluginMarket` marks exactly two methods, `listIndexes` and
//! `setIndexes`, both of which this build already serves.
//!
//! The annotator that uses these is a private helper:
//!
//! ```typescript
//! private async annotator() {
//!   const [bundles, { exemptions }] = [
//!     await this.ctx.pluginManager.listBundles(),
//!     this.ctx.pluginManager.listVersionExemptions(),
//!   ]
//!   const runtimeVersion = getDshRuntimeVersion()
//!   return plugin => listingOf(plugin, bundles, exemptions, runtimeVersion)
//! }
//! ```
//!
//! So the honest statement is: this module is a faithful port of a rule set that nothing in the 126
//! endpoints reaches. Serving it would mean **inventing an endpoint upstream does not have** -- a
//! `pluginMarket.list` -- which is a product decision, not a porting one, and one that would produce a
//! client-visible surface with no upstream counterpart to compare against.
//!
//! What is *not* blocked: `pluginManager.listBundles` and `listVersionExemptions` are both registered
//! here, so the inputs this module needs already exist as endpoints. Only the assembly is absent, and
//! upstream has not assembled it either.

#![allow(
    dead_code,
    reason = "this module is compiled privately by the `#[path]` test until lib.rs re-exports it; every item here is public surface for that wiring, so 'unused within this test binary' is not dead code"
)]

use std::cmp::Ordering;
use std::collections::BTreeMap;

use crate::boot_gap::{SemVer, pinned_git_origin};
use crate::plugin_market_gap::{
    MarketInstallTarget, MarketPlugin, MarketVersion, market_install_target,
};

// ============================================================================================
// Semver ranges — upstream `node-semver`'s `Range`, as `evaluatePluginCompatibility` uses it
// ============================================================================================
//
// `satisfies(runtimeVersion, range)` in `plugin-compatibility.ts` is
//
//     range.trim() !== '' && semver.satisfies(runtimeVersion, range, { includePrerelease: true })
//
// The grammar below is `node-semver` 7's, reproduced branch for branch: `||`-separated
// alternatives, whitespace-separated comparators, hyphen ranges, x-ranges, tilde and caret ranges,
// and stars. `includePrerelease: true` is not a detail that can be dropped: it makes every
// desugared lower bound `-0` (so `^1.2` means `>=1.2.0-0 <2.0.0-0`) and it disables the "a
// prerelease only matches a comparator that names a prerelease on the same tuple" filter. A
// version with a prerelease tag is exactly what DSH's own releases carry, so getting this wrong
// would refuse the running build's plugins.

/// A comparator's operator. `=` is spelled `''` by `node-semver`, and both compare as equality.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum CompOp {
    /// `=` or a bare version.
    Eq,
    /// `<`.
    Lt,
    /// `<=`.
    Le,
    /// `>`.
    Gt,
    /// `>=`.
    Ge,
}

impl CompOp {
    /// The operator as `node-semver` writes it in a comparator's `value`.
    fn as_str(self) -> &'static str {
        match self {
            Self::Eq => "",
            Self::Lt => "<",
            Self::Le => "<=",
            Self::Gt => ">",
            Self::Ge => ">=",
        }
    }
}

/// Read a leading `GTLT` (`((?:<|>)?=?)`), returning the operator and what follows it.
fn take_operator(text: &str) -> (CompOp, &str) {
    for (prefix, op) in [
        (">=", CompOp::Ge),
        ("<=", CompOp::Le),
        (">", CompOp::Gt),
        ("<", CompOp::Lt),
        ("=", CompOp::Eq),
    ] {
        if let Some(rest) = text.strip_prefix(prefix) {
            return (op, rest);
        }
    }
    (CompOp::Eq, text)
}

/// One desugared bound: an operator and the version it compares against. A comparator with no
/// version is `node-semver`'s `ANY`, which every version satisfies.
#[derive(Clone, Debug)]
struct Comparator {
    /// The comparison to perform.
    op: CompOp,
    /// The bound; `None` is `ANY`.
    version: Option<SemVer>,
}

impl Comparator {
    /// An `ANY` comparator (`''`, `*`, and every star form desugar to it).
    fn any() -> Self {
        Self {
            op: CompOp::Eq,
            version: None,
        }
    }

    /// Parse one desugared comparator — upstream `new Comparator(comp)`.
    ///
    /// `node-semver`'s strict `COMPARATOR` pattern is `^GTLT\s*(v?M.m.p(-pre)?(+build)?)$|^$`: a
    /// bare operator with no version is *not* the empty comparator, it is an invalid one, and so is
    /// an operator in front of something that is not a full version.
    fn parse(text: &str) -> Option<Self> {
        let text = text.trim();
        if text.is_empty() {
            return Some(Self::any());
        }
        let (op, rest) = take_operator(text);
        Some(Self {
            op,
            version: Some(SemVer::parse(rest.trim())?),
        })
    }

    /// Whether `version` passes this bound — upstream `Comparator.test`.
    fn test(&self, version: &SemVer) -> bool {
        let Some(bound) = &self.version else {
            return true;
        };
        let ordering = version.cmp(bound);
        match self.op {
            CompOp::Eq => ordering == Ordering::Equal,
            CompOp::Lt => ordering == Ordering::Less,
            CompOp::Le => ordering != Ordering::Greater,
            CompOp::Gt => ordering == Ordering::Greater,
            CompOp::Ge => ordering != Ordering::Less,
        }
    }

    /// Whether this comparator is `node-semver`'s null set (`<0.0.0-0`), which nothing satisfies.
    fn is_null_set(&self) -> bool {
        self.op == CompOp::Lt
            && self
                .version
                .as_ref()
                .is_some_and(|version| version.to_string() == "0.0.0-0")
    }
}

/// A version as written inside a range: `M`, `M.m`, `M.m.p`, each possibly `x`/`X`/`*`, plus an
/// optional prerelease — upstream `XRANGEPLAIN`.
///
/// An absent component and an explicit wildcard are the same thing to `node-semver` (`isX` treats
/// `undefined`, `x`, `X` and `*` alike), so both are `None` here.
#[derive(Clone, Debug)]
struct XRange {
    /// The numbers written, with `None` for a wildcard or an omitted tail.
    major: Option<u64>,
    minor: Option<u64>,
    patch: Option<u64>,
    /// The prerelease text after `-`, when written.
    prerelease: Option<String>,
}

/// Whether a range identifier is a wildcard — upstream `isX`.
fn is_x(component: Option<u64>) -> bool {
    component.is_none()
}

/// Read one `XRANGEIDENTIFIER` (`0|[1-9]\d*|x|X|\*`), returning it and the remainder.
fn take_x_identifier(text: &str) -> Option<(Option<u64>, &str)> {
    let first = text.chars().next()?;
    if matches!(first, 'x' | 'X' | '*') {
        return Some((None, &text[1..]));
    }
    if !first.is_ascii_digit() {
        return None;
    }
    let end = text
        .find(|ch: char| !ch.is_ascii_digit())
        .unwrap_or(text.len());
    let digits = &text[..end];
    // `0|[1-9]\d*`: a leading zero is not a version identifier, it is a failed match.
    if digits.len() > 1 && digits.starts_with('0') {
        return None;
    }
    Some((Some(digits.parse::<u64>().ok()?), &text[end..]))
}

/// Whether one dot-separated prerelease identifier is `NONNUMERICIDENTIFIER` or a canonical
/// `NUMERICIDENTIFIER`.
fn is_prerelease_identifier(text: &str) -> bool {
    if text.is_empty()
        || !text
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-')
    {
        return false;
    }
    let numeric = text.bytes().all(|byte| byte.is_ascii_digit());
    if numeric {
        // `0|[1-9]\d*`: `01` matches neither alternative.
        return text == "0" || !text.starts_with('0');
    }
    // `\d*[a-zA-Z-][a-zA-Z0-9-]*`: at least one letter or hyphen, after any digits.
    text.bytes()
        .any(|byte| byte.is_ascii_alphabetic() || byte == b'-')
}

/// Read a prerelease body (`identifier(.identifier)*`) up to the next character that cannot be in
/// one, returning the text and the remainder.
fn take_prerelease_body(text: &str) -> Option<(String, &str)> {
    let end = text
        .find(|ch: char| !(ch.is_ascii_alphanumeric() || ch == '-' || ch == '.'))
        .unwrap_or(text.len());
    let body = &text[..end];
    if body.is_empty() || !body.split('.').all(is_prerelease_identifier) {
        return None;
    }
    Some((body.to_owned(), &text[end..]))
}

/// Read build metadata (`+identifier(.identifier)*`) and discard it — semver precedence ignores it.
fn take_build_body(text: &str) -> Option<&str> {
    let end = text
        .find(|ch: char| !(ch.is_ascii_alphanumeric() || ch == '-' || ch == '.'))
        .unwrap_or(text.len());
    let body = &text[..end];
    if body.is_empty() || !body.split('.').all(|part| !part.is_empty()) {
        return None;
    }
    Some(&text[end..])
}

/// Read `[v=\s]*XRANGEPLAIN`, returning the range and the remainder.
fn take_prefixed_xrange(text: &str) -> Option<(XRange, &str)> {
    let rest = text.trim_start_matches(['v', '=']);
    let (major, rest) = take_x_identifier(rest)?;
    let mut range = XRange {
        major,
        minor: None,
        patch: None,
        prerelease: None,
    };
    // `M` alone: the trailing groups are optional.
    let Some(rest) = rest.strip_prefix('.') else {
        return Some((range, rest));
    };
    let (minor, rest) = take_x_identifier(rest)?;
    range.minor = minor;
    let Some(rest) = rest.strip_prefix('.') else {
        return Some((range, rest));
    };
    let (patch, rest) = take_x_identifier(rest)?;
    range.patch = patch;
    let rest = match rest.strip_prefix('-') {
        Some(after) => {
            let (prerelease, rest) = take_prerelease_body(after)?;
            range.prerelease = Some(prerelease);
            rest
        }
        None => rest,
    };
    let rest = match rest.strip_prefix('+') {
        Some(after) => take_build_body(after)?,
        None => rest,
    };
    Some((range, rest))
}

/// Match a whole token against `^GTLT\s*XRANGEPLAIN$` — upstream `XRANGE`.
fn xrange_match(text: &str) -> Option<XRange> {
    let (_, rest) = take_operator(text);
    let (range, rest) = take_prefixed_xrange(rest.trim_start())?;
    if rest.is_empty() { Some(range) } else { None }
}

/// Match a whole token against `^(?:~>?)XRANGEPLAIN$` — upstream `TILDE`.
fn tilde_match(text: &str) -> Option<XRange> {
    let rest = text.strip_prefix("~>").or_else(|| text.strip_prefix('~'))?;
    let (range, rest) = take_prefixed_xrange(rest)?;
    if rest.is_empty() { Some(range) } else { None }
}

/// Match a whole token against `^(?:\^)XRANGEPLAIN$` — upstream `CARET`.
fn caret_match(text: &str) -> Option<XRange> {
    let rest = text.strip_prefix('^')?;
    let (range, rest) = take_prefixed_xrange(rest)?;
    if rest.is_empty() { Some(range) } else { None }
}

/// Remove every `+build` occurrence — upstream `parseRange`'s global `BUILDSTRIPRE`.
///
/// The pattern demands at least one build identifier, so a dangling `+` is *not* stripped: it is
/// left for the comparator parser to refuse, which is how `1.2.3+` comes to be an invalid range
/// rather than a valid `1.2.3`.
fn strip_build(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    while let Some(plus) = rest.find('+') {
        out.push_str(&rest[..plus]);
        let after = &rest[plus + 1..];
        let end = after
            .find(|ch: char| !(ch.is_ascii_alphanumeric() || ch == '-' || ch == '.'))
            .unwrap_or(after.len());
        if end == 0 || after[..end].split('.').any(str::is_empty) {
            // Not build metadata: keep the `+` so the token fails to parse with it.
            out.push('+');
            rest = after;
            continue;
        }
        rest = &after[end..];
    }
    out.push_str(rest);
    out
}

/// Rewrite `from - to` into `>=from <to` — upstream `hyphenReplace`.
///
/// The pattern is anchored to the whole alternative and requires whitespace on both sides of the
/// hyphen, so `1.2.3-alpha - 2.0.0` is a hyphen range while `1.2.3-alpha` is one version.
fn hyphen_replace(text: &str) -> String {
    let trimmed = text.trim();
    let Some((left, right)) = split_hyphen_range(trimmed) else {
        return text.to_owned();
    };
    let (Some(low), Some(high)) = (
        xrange_plain_match(left.trim()),
        xrange_plain_match(right.trim()),
    ) else {
        return text.to_owned();
    };
    let from = if is_x(low.major) {
        String::new()
    } else if is_x(low.minor) {
        format!(">={}.0.0-0", low.major.unwrap_or(0))
    } else if is_x(low.patch) {
        format!(
            ">={}.{}.0-0",
            low.major.unwrap_or(0),
            low.minor.unwrap_or(0)
        )
    } else if low.prerelease.is_some() {
        format!(">={}", left.trim())
    } else {
        format!(">={}-0", left.trim())
    };
    let to = if is_x(high.major) {
        String::new()
    } else if is_x(high.minor) {
        format!("<{}.0.0-0", high.major.unwrap_or(0) + 1)
    } else if is_x(high.patch) {
        format!(
            "<{}.{}.0-0",
            high.major.unwrap_or(0),
            high.minor.unwrap_or(0) + 1
        )
    } else if let Some(prerelease) = &high.prerelease {
        format!(
            "<={}.{}.{}-{prerelease}",
            high.major.unwrap_or(0),
            high.minor.unwrap_or(0),
            high.patch.unwrap_or(0)
        )
    } else {
        // `includePrerelease`, so an inclusive upper bound becomes the next tuple's `-0`.
        format!(
            "<{}.{}.{}-0",
            high.major.unwrap_or(0),
            high.minor.unwrap_or(0),
            high.patch.unwrap_or(0) + 1
        )
    };
    format!("{from} {to}").trim().to_owned()
}

/// Match a whole string against `XRANGEPLAIN` (no operator) — the halves of a hyphen range.
fn xrange_plain_match(text: &str) -> Option<XRange> {
    let (range, rest) = take_prefixed_xrange(text)?;
    if rest.is_empty() { Some(range) } else { None }
}

/// Split `from - to` at its single hyphen separator, if it has exactly one.
fn split_hyphen_range(text: &str) -> Option<(&str, &str)> {
    let bytes = text.as_bytes();
    let mut found = None;
    for (index, byte) in bytes.iter().enumerate() {
        if *byte != b'-' {
            continue;
        }
        let before = text[..index].chars().next_back();
        let after = text[index + 1..].chars().next();
        if before.is_some_and(char::is_whitespace) && after.is_some_and(char::is_whitespace) {
            if found.is_some() {
                // Two separators is not one hyphen range.
                return None;
            }
            found = Some(index);
        }
    }
    let index = found?;
    Some((&text[..index], &text[index + 1..]))
}

/// Desugar one whitespace-delimited token into comparators — upstream `parseComparator`.
///
/// The passes are a *chain*, not a choice: `parseComparator` applies carets, then tildes, then
/// x-ranges, then stars to the same token. Chaining matters for `*.*.*` and `>=*`, which the
/// x-range pass rewrites to `*` and only the star pass turns into `ANY`.
///
/// `None` means the token is not a well-formed range at all (`x.1`), which `node-semver` signals
/// by leaving it for the comparator parser to refuse.
fn desugar(token: &str) -> Option<String> {
    let text = strip_build(token);
    let text = if let Some(range) = caret_match(&text) {
        replace_caret(&range)
    } else if let Some(range) = tilde_match(&text) {
        replace_tilde(&range)
    } else if let Some(range) = xrange_match(&text) {
        replace_xrange(&text, &range)?
    } else {
        text
    };
    Some(replace_stars(&text))
}

/// Expand `^…` — upstream `replaceCaret`, with `includePrerelease`'s `-0` lower bounds.
fn replace_caret(range: &XRange) -> String {
    let Some(major) = range.major else {
        return String::new();
    };
    let Some(minor) = range.minor else {
        return format!(">={major}.0.0-0 <{}.0.0-0", major + 1);
    };
    let Some(patch) = range.patch else {
        return if major == 0 {
            format!(">={major}.{minor}.0-0 <{major}.{}.0-0", minor + 1)
        } else {
            format!(">={major}.{minor}.0-0 <{}.0.0-0", major + 1)
        };
    };
    match &range.prerelease {
        Some(prerelease) => {
            if major == 0 {
                if minor == 0 {
                    format!(
                        ">={major}.{minor}.{patch}-{prerelease} <{major}.{minor}.{}-0",
                        patch + 1
                    )
                } else {
                    format!(
                        ">={major}.{minor}.{patch}-{prerelease} <{major}.{}.0-0",
                        minor + 1
                    )
                }
            } else {
                format!(
                    ">={major}.{minor}.{patch}-{prerelease} <{}.0.0-0",
                    major + 1
                )
            }
        }
        None => {
            if major == 0 {
                if minor == 0 {
                    format!(">={major}.{minor}.{patch} <{major}.{minor}.{}-0", patch + 1)
                } else {
                    format!(">={major}.{minor}.{patch} <{major}.{}.0-0", minor + 1)
                }
            } else {
                format!(">={major}.{minor}.{patch} <{}.0.0-0", major + 1)
            }
        }
    }
}

/// Expand `~…` — upstream `replaceTilde`, with `includePrerelease`'s `-0` lower bounds.
fn replace_tilde(range: &XRange) -> String {
    let Some(major) = range.major else {
        return String::new();
    };
    let Some(minor) = range.minor else {
        return format!(">={major}.0.0-0 <{}.0.0-0", major + 1);
    };
    let Some(patch) = range.patch else {
        return format!(">={major}.{minor}.0-0 <{major}.{}.0-0", minor + 1);
    };
    match &range.prerelease {
        Some(prerelease) => format!(
            ">={major}.{minor}.{patch}-{prerelease} <{major}.{}.0-0",
            minor + 1
        ),
        None => format!(">={major}.{minor}.{patch} <{major}.{}.0-0", minor + 1),
    }
}

/// Expand an x-range (`1.x`, `>1.2`, `1.2.3`) — upstream `replaceXRange`.
///
/// `None` means the token is not a well-formed x-range (`x.1`, `1.x.3`): `node-semver` leaves it
/// untouched and the comparator parser then refuses it, so the whole range is invalid.
fn replace_xrange(text: &str, range: &XRange) -> Option<String> {
    let x_major = is_x(range.major);
    // `invalidXRangeOrder`: `x.1` and `1.x.3` are not ranges.
    if (x_major && !is_x(range.minor)) || (is_x(range.minor) && range.patch.is_some()) {
        return None;
    }
    let (op, _) = take_operator(text);
    let x_minor = x_major || is_x(range.minor);
    let x_patch = x_minor || is_x(range.patch);
    if !x_patch {
        // Fully specified: the token stands as written.
        return Some(text.to_owned());
    }
    if x_major {
        return Some(if matches!(op, CompOp::Gt | CompOp::Lt) {
            "<0.0.0-0".to_owned()
        } else {
            "*".to_owned()
        });
    }
    let mut major = range.major.unwrap_or(0);
    let mut minor = range.minor.unwrap_or(0);
    let (op, extra) = if matches!(op, CompOp::Gt | CompOp::Lt | CompOp::Ge | CompOp::Le) {
        if x_minor {
            minor = 0;
        }
        match op {
            CompOp::Gt => {
                if x_minor {
                    major += 1;
                    minor = 0;
                } else {
                    minor += 1;
                }
                (CompOp::Ge, "0")
            }
            CompOp::Le => {
                if x_minor {
                    major += 1;
                } else {
                    minor += 1;
                }
                (CompOp::Lt, "0")
            }
            other => (other, "0"),
        }
    } else if x_minor {
        return Some(format!(">={major}.0.0-0 <{}.0.0-0", major + 1));
    } else {
        return Some(format!(
            ">={major}.{minor}.0-0 <{}.{}.0-0",
            major,
            minor + 1
        ));
    };
    Some(format!("{}{major}.{minor}.{extra}-0", op.as_str()))
}

/// Remove star comparators — upstream `replaceStars`, whose pattern is `(<|>)?=?\s*\*`.
///
/// The star and any operator glued to it disappear, leaving whatever surrounded them:
/// `>=2.0.0 *` becomes `>=2.0.0`, and `*` alone becomes `''`, which is `ANY`.
fn replace_stars(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    while let Some(star) = rest.find('*') {
        let mut keep = star;
        // The pattern is not anchored, so `>`/`<` and `=` directly before the star are swallowed
        // with it, and any whitespace between them and the star (`   *`).
        while keep > 0 && rest.as_bytes()[keep - 1].is_ascii_whitespace() {
            keep -= 1;
        }
        if keep > 0 && rest.as_bytes()[keep - 1] == b'=' {
            keep -= 1;
        }
        if keep > 0 && matches!(rest.as_bytes()[keep - 1], b'<' | b'>') {
            keep -= 1;
        }
        out.push_str(&rest[..keep]);
        rest = &rest[star + 1..];
    }
    out.push_str(rest);
    out
}

/// Drop `>=0.0.0-0`, which `node-semver` treats as `*` — upstream `replaceGTE0` with
/// `includePrerelease`.
fn replace_gte0(text: &str) -> &str {
    if text.trim() == ">=0.0.0-0" { "" } else { text }
}

/// Collapse every whitespace run to one space and trim — `Range`'s own normalization.
fn collapse_whitespace(text: &str) -> String {
    text.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// Split one alternative into tokens, joining an operator to the version it modifies, which is
/// what `COMPARATORTRIM`, `TILDETRIM` and `CARETTRIM` do to `> 1.2.3`, `~ 1.2` and `^ 1.2`.
fn merge_operators(text: &str) -> Vec<String> {
    /// Whether a token is nothing but an operator, waiting for its version.
    fn is_bare_operator(token: &str) -> bool {
        matches!(token, "<" | ">" | "<=" | ">=" | "=" | "~" | "~>" | "^")
    }
    let mut tokens: Vec<String> = Vec::new();
    for token in text.split_whitespace() {
        match tokens.last_mut() {
            Some(previous) if is_bare_operator(previous) && !is_bare_operator(token) => {
                let combined = format!("{previous}{token}");
                *previous = combined;
            }
            _ => tokens.push(token.to_owned()),
        }
    }
    tokens
}

/// Parse one `||` alternative into its comparator set — upstream `Range.parseRange`.
///
/// Returns `None` where `node-semver` throws, which is what makes [`satisfies`] answer `false` for
/// a malformed range instead of propagating an error.
fn parse_comparators(range: &str) -> Option<Vec<Comparator>> {
    let text = strip_build(range);
    let text = hyphen_replace(&text);
    let tokens = merge_operators(&text);
    let mut expanded: Vec<String> = Vec::new();
    if tokens.is_empty() {
        // An empty alternative is `ANY`, not an empty set.
        expanded.push(String::new());
    }
    for token in &tokens {
        let desugared = desugar(token)?;
        let mut pieces = desugared.split_whitespace().peekable();
        if pieces.peek().is_none() {
            // A desugaring that produced nothing (`*`, `^x`) is `ANY`.
            expanded.push(String::new());
        }
        expanded.extend(pieces.map(str::to_owned));
    }
    let mut comparators = Vec::with_capacity(expanded.len());
    for piece in &expanded {
        comparators.push(Comparator::parse(replace_gte0(piece))?);
    }
    if comparators.iter().any(Comparator::is_null_set) {
        return Some(vec![Comparator::parse("<0.0.0-0")?]);
    }
    Some(comparators)
}

/// Parse a whole range into its alternatives — upstream `new Range(range, options)`.
fn parse_range(range: &str) -> Option<Vec<Vec<Comparator>>> {
    let collapsed = collapse_whitespace(range);
    let mut sets = Vec::new();
    for alternative in collapsed.split("||") {
        sets.push(parse_comparators(alternative.trim())?);
    }
    if sets.is_empty() { None } else { Some(sets) }
}

/// Whether the running version satisfies a range, prereleases included — upstream
/// `plugin-compatibility.ts`'s `satisfies`, i.e. `semver.satisfies(v, r, { includePrerelease })`.
///
/// An unparsable range or runtime version answers `false`, as `semver.satisfies` does when the
/// `Range` constructor or the version parse throws. Note that this is `node-semver`'s rule and
/// not the whole upstream requirement rule: `plugin-compatibility.ts` additionally refuses an
/// empty or blank range before calling this, and [`compatibility_of`] carries that half.
///
/// ```
/// # use girder_core::market_compat_gap::satisfies;
/// assert!(satisfies("1.4.2", "^1.2.3"));
/// assert!(satisfies("1.5.0-rc.1", "^1.2.3"));   // prereleases participate
/// assert!(!satisfies("2.0.0-rc.1", "^1.2.3"));  // but `^1.2.3` still excludes the next major
/// assert!(!satisfies("1.4.2", "not a range"));
/// ```
#[must_use]
pub fn satisfies(runtime_version: &str, range: &str) -> bool {
    let Some(version) = SemVer::parse(runtime_version) else {
        return false;
    };
    let Some(sets) = parse_range(range) else {
        return false;
    };
    sets.iter()
        .any(|set| set.iter().all(|comparator| comparator.test(&version)))
}

// ============================================================================================
// Compatibility — upstream `compatibilityOf`, `MarketCompatibility`
// ============================================================================================

/// The banner key of the only requirement a market version can carry — `engines.dsh`.
pub const ENGINES_DSH: &str = "engines.dsh";

/// Why a listed version does not run on this DSH — upstream the `compatible: false` arm of
/// `MarketCompatibility`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MarketIncompatibility {
    /// The running DSH version the range was evaluated against.
    pub runtime_version: String,
    /// The unmet requirements. A market version declares only `engines.dsh`, so this map holds at
    /// most that one key — upstream's shape is a record because its evaluator also serves
    /// `peerDependencies`, which a synthesised market manifest never has.
    pub requirements: BTreeMap<String, String>,
    /// Whether the profile holds an exemption for this exact package *and* runtime version.
    pub exempted: bool,
}

/// Whether a listed version runs on this DSH — upstream `MarketCompatibility`.
///
/// An exempted mismatch is still `Incompatible`: the exemption records informed consent, and the
/// listing says so rather than pretending the range was met.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum MarketCompatibility {
    /// Every requirement is met (or none is declared).
    Compatible,
    /// A requirement is unmet, with the evidence.
    Incompatible(MarketIncompatibility),
}

impl MarketCompatibility {
    /// Whether the version may be installed without an exemption.
    #[must_use]
    pub fn is_compatible(&self) -> bool {
        matches!(self, Self::Compatible)
    }
}

/// Whether one listed version satisfies the running DSH — upstream `compatibilityOf`.
///
/// The manifest upstream synthesises is `{ name, version, engines: { dsh } }`, so the only input
/// that decides the answer is the version's `engines.dsh` range; an absent range means every
/// runtime. `exemptions` is exactly `CompatibilityFile::exemptions` from `boot_gap`, keyed
/// `name@version` and holding the DSH versions a person allowed.
///
/// A blank range is *unmet*, not "any version": upstream's helper tests `range.trim() !== ''`
/// before delegating to `semver.satisfies`, because an empty `engines.dsh` is a broken declaration
/// rather than a permissive one.
#[must_use]
pub fn compatibility_of(
    name: &str,
    version: &MarketVersion,
    exemptions: &BTreeMap<String, Vec<String>>,
    runtime_version: &str,
) -> MarketCompatibility {
    let Some(range) = version.engines.dsh.as_deref() else {
        return MarketCompatibility::Compatible;
    };
    if !range.trim().is_empty() && satisfies(runtime_version, range) {
        return MarketCompatibility::Compatible;
    }
    let exempted = exemptions
        .get(&format!("{name}@{}", version.version))
        .is_some_and(|versions| versions.iter().any(|allowed| allowed == runtime_version));
    MarketCompatibility::Incompatible(MarketIncompatibility {
        runtime_version: runtime_version.to_owned(),
        requirements: BTreeMap::from([(ENGINES_DSH.to_owned(), range.to_owned())]),
        exempted,
    })
}

// ============================================================================================
// Installation — upstream `installationOf`, `MarketInstallation`
// ============================================================================================

/// How a plugin relates to the profile's bundles — upstream `MarketInstallationStatus`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MarketInstallationStatus {
    /// The profile does not list the package.
    NotInstalled,
    /// The profile installed it at exactly the measured version.
    Installed,
    /// The profile installed an older version than the measured one.
    UpdateAvailable,
    /// The profile installed a version that is unparsable, or greater than the measured one.
    DifferentVersion,
    /// The DSH installation ships the package; the profile did not install it.
    Provided,
}

impl MarketInstallationStatus {
    /// The status as the Remote surface spells it.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::NotInstalled => "not-installed",
            Self::Installed => "installed",
            Self::UpdateAvailable => "update-available",
            Self::DifferentVersion => "different-version",
            Self::Provided => "provided",
        }
    }
}

/// A plugin's installation status and the installed version when the profile reports one —
/// upstream `MarketInstallation`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MarketInstallation {
    /// How the package relates to the profile.
    pub status: MarketInstallationStatus,
    /// The installed version, when one is known.
    pub installed_version: Option<String>,
}

impl MarketInstallation {
    /// A convenience constructor for the two field-less statuses.
    fn status(status: MarketInstallationStatus) -> Self {
        Self {
            status,
            installed_version: None,
        }
    }
}

/// How `updateBundle` replaces an installed copy — upstream `BundleInfo['update']`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BundleUpdate {
    /// A registry dependency, whose newer versions `listVersions` reads.
    Registry,
    /// A Git copy, which only updates to another commit of the same repository directory.
    Git,
}

/// Where a bundle comes from in this installation — upstream `BundleInfo['availability']`.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum BundleAvailability {
    /// The DSH installation ships it as an optional bundle.
    Installation,
    /// The profile installed it, or ships it selected.
    #[default]
    Profile,
    /// The package cannot be found.
    Missing,
}

/// The profile's facts about one bundle — the subset of upstream `BundleInfo` this projection
/// reads.
///
/// `#[non_exhaustive]` is not needed: the fields are named as upstream names them, so a caller
/// converting a real `BundleInfo` assigns them one by one and a new upstream field is a new field
/// here rather than a silent misread.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct BundleFacts {
    /// The package name; the only field that matches a market plugin.
    pub name: String,
    /// Whether the profile selected or installed the package.
    pub installed: bool,
    /// Where the package comes from.
    pub availability: BundleAvailability,
    /// The manifest version the profile reports, when it reports one.
    pub version: Option<String>,
    /// How `updateBundle` replaces it, absent when it cannot.
    pub update: Option<BundleUpdate>,
    /// The dependency spec the profile records, for a package it installed.
    pub source: Option<String>,
}

/// How the profile relates to one version — upstream `installationOf`.
///
/// `version` is the *measured* version: the plugin's default version, or its highest listed one
/// when no version is compatible.
#[must_use]
pub fn installation_of(bundle: Option<&BundleFacts>, version: &str) -> MarketInstallation {
    let Some(bundle) = bundle else {
        // An on-demand catalog entry is listed before the profile installs it.
        return MarketInstallation::status(MarketInstallationStatus::NotInstalled);
    };
    if !bundle.installed {
        if bundle.availability != BundleAvailability::Installation {
            return MarketInstallation::status(MarketInstallationStatus::NotInstalled);
        }
        return MarketInstallation {
            status: MarketInstallationStatus::Provided,
            installed_version: bundle.version.clone(),
        };
    }
    let Some(installed) = bundle.version.as_deref() else {
        return MarketInstallation::status(MarketInstallationStatus::DifferentVersion);
    };
    if installed == version {
        return MarketInstallation {
            status: MarketInstallationStatus::Installed,
            installed_version: Some(installed.to_owned()),
        };
    }
    // An unparsable installed version — a local `link:` copy, say — cannot be ordered, so the
    // measured version is merely different rather than newer.
    let newer = match (SemVer::parse(version), SemVer::parse(installed)) {
        (Some(measured), Some(installed)) => measured > installed,
        _ => false,
    };
    MarketInstallation {
        status: if newer {
            MarketInstallationStatus::UpdateAvailable
        } else {
            MarketInstallationStatus::DifferentVersion
        },
        installed_version: Some(installed.to_owned()),
    }
}

// ============================================================================================
// The listing — upstream `listingOf`, `MarketListingEntry`, `MarketListingVersion`
// ============================================================================================

/// One listed version with the arguments that install it — upstream `MarketListingVersion`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MarketListingVersion {
    /// The index's own version record.
    pub version: MarketVersion,
    /// The spec `pnpm add` accepts for it.
    pub install: MarketInstallTarget,
    /// Whether the running DSH accepts it.
    pub compatibility: MarketCompatibility,
    /// Whether `updateBundle` accepts it for the installed copy: the copy came from a Git commit,
    /// this version is newer, and it pins a commit in the same repository *and* `path:` directory.
    pub update: bool,
}

/// One index plugin with the profile-specific facts a market list displays — upstream
/// `MarketListingEntry`.
///
/// Not `Eq`: the embedded [`MarketPlugin`] carries `f64` sort-key values.
#[derive(Clone, Debug, PartialEq)]
pub struct MarketListingEntry {
    /// The index record, with its unannotated version list.
    pub plugin: MarketPlugin,
    /// Every version, in the index's (descending precedence) order, annotated.
    pub versions: Vec<MarketListingVersion>,
    /// The version a plain install takes: the highest compatible version without a prerelease
    /// tag, or the highest compatible prerelease when the plugin lists no stable version at all.
    /// Absent when no version is compatible.
    pub default_version: Option<String>,
    /// How the profile relates to `default_version`, or to the highest listed version.
    pub installation: MarketInstallation,
}

/// Annotate one index plugin with the profile's bundles, exemptions, and running version —
/// upstream `listingOf`.
///
/// Everything the answer depends on is a parameter, so the same inputs always produce the same
/// listing on any machine. The service wrapper that reads `listBundles`, the exemption file and
/// the runtime version is what this deliberately is not.
#[must_use]
pub fn listing_of(
    plugin: &MarketPlugin,
    bundles: &[BundleFacts],
    exemptions: &BTreeMap<String, Vec<String>>,
    runtime_version: &str,
) -> MarketListingEntry {
    let bundle = bundles.iter().find(|bundle| bundle.name == plugin.name);
    // A Git copy updates only to another commit of the repository directory it came from.
    let origin = match bundle {
        Some(bundle) if bundle.update == Some(BundleUpdate::Git) => {
            bundle.source.as_deref().and_then(pinned_git_origin)
        }
        _ => None,
    };
    let installed_version = bundle.and_then(|bundle| bundle.version.as_deref());
    let versions: Vec<MarketListingVersion> = plugin
        .versions
        .iter()
        .map(|version| {
            let install = market_install_target(&plugin.name, version);
            let update = origin.is_some()
                && installed_version.is_some_and(|installed| is_newer(&version.version, installed))
                && pinned_git_origin(&install.spec) == origin;
            MarketListingVersion {
                version: version.clone(),
                install,
                compatibility: compatibility_of(&plugin.name, version, exemptions, runtime_version),
                update,
            }
        })
        .collect();
    // `stable` looks at *every* listed version, not only the compatible ones: a plugin with one
    // stable release at any version defaults to a stable one, and only a plugin with no stable
    // release at all may default to a prerelease.
    let stable = plugin
        .versions
        .iter()
        .any(|version| !is_prerelease(&version.version));
    let default_version = versions
        .iter()
        .find(|version| {
            version.compatibility.is_compatible() && (!stable || !is_prerelease_version(version))
        })
        .map(|version| version.version.version.clone());
    let measured = default_version
        .clone()
        .or_else(|| {
            plugin
                .versions
                .first()
                .map(|version| version.version.clone())
        })
        .unwrap_or_default();
    MarketListingEntry {
        plugin: plugin.clone(),
        versions,
        default_version,
        installation: installation_of(bundle, &measured),
    }
}

/// Whether a listed version carries a prerelease tag, for the default-version rule.
fn is_prerelease_version(version: &MarketListingVersion) -> bool {
    is_prerelease(&version.version.version)
}

/// Whether a version string carries a prerelease tag — upstream `semver.prerelease(v) !== null`.
///
/// An unparsable version counts as "not a prerelease", matching `semver.prerelease`'s `null`.
fn is_prerelease(version: &str) -> bool {
    SemVer::parse(version).is_some_and(|parsed| parsed.is_prerelease())
}

/// Whether `candidate` is newer than `installed` by semver precedence — upstream `semver.gt`.
///
/// An unparsable pair answers `false` where upstream would throw: a listing is a read, and an
/// index version that does not parse must not take the page down with it.
fn is_newer(candidate: &str, installed: &str) -> bool {
    match (SemVer::parse(candidate), SemVer::parse(installed)) {
        (Some(candidate), Some(installed)) => candidate > installed,
        _ => false,
    }
}
