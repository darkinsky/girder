//! The plugin market's *decision* half: format-1 index records, the text search, filter, and order
//! over them, the index-source URL rule, and the install arguments one listed version produces.
//!
//! # What upstream `packages/boot/plugin-market` contains, measured
//!
//! 11 files, 1,858 lines. Split by whether a function's answer depends on the world outside the
//! process:
//!
//! | Upstream | Lines | Kind | Where the work goes |
//! | --- | --- | --- | --- |
//! | `search.ts` | 155 | **pure** | `MiniSearch` over plugin documents; BM25, prefix and fuzzy matching, sort order |
//! | `index-url.ts` | 26 | **pure** | `normalizeIndexUrl`: http(s) only, no credentials, no fragment |
//! | `parse.ts` | 43 | mixed (zero awaits) | strict UTF-8 → JSON → `parseMarketIndex`, all in-memory |
//! | `types.ts` | 176 | declarations | the wire records, no behaviour |
//! | `index.ts` (decisions) | ~120 of 520 | **pure** | `listingOf`/`installationOf`/`compatibilityOf`, page-range validation |
//! | `index.ts` (service) | ~400 of 520 | delegated I/O | `fetch`, cache TTLs, the Worker hand-off, `settings.update` |
//! | `repository-cache.ts` | 432 | delegated I/O | 44 `await`/`spawn`/`fetch` sites: `git` partial clones, LRU eviction |
//! | `detail.ts` | 221 | delegated I/O | `hosted-git-info`, document reads; only `repositoryOf`/`manifestRepository` are pure |
//! | `detail-files.ts` | 79 | delegated I/O | the GitLab files API, npm CDN reads, README probing |
//! | `tools.ts` | 141 | delegated I/O | an LLM tool surface over the service |
//! | `parse-thread.ts`, `worker.ts` | 65 | delegated I/O | Worker Thread lifecycle |
//!
//! And the validator those wrappers call, `packages/util/plugin-market-index/src/index.ts`
//! (334 lines, **pure**): `parseMarketIndex` and `marketInstallTarget`.
//!
//! # Why the search really is pure, and what the `MiniSearch` dependency costs
//!
//! `MarketSearch` looks like an integration, but `MiniSearch` is a **deterministic in-memory
//! index**: `search.select()` reads nothing but `this.index.plugins`. The only impurity in
//! `search.ts` is `addAllAsync`'s chunked yielding between 500-document batches, which exists to
//! keep the *Node event loop* responsive and has no effect on the result. So the search semantics
//! are portable — but only if the replacement reproduces them, and `MiniSearch` is not a black box
//! to approximate: it is BM25 with fixed parameters (`k=1.2, b=0.7, d=0.5`), field boosts
//! (`name`/`title` 3, `keywords` 2, the rest 1), prefix weight `0.375`, fuzzy weight `0.45`,
//! `maxFuzzy` 2, and `AND` across query terms. A "substring match, ordered by position" stand-in
//! would not be a port; it would be a different product behaviour. So this module reimplements the
//! scoring formula, the prefix and fuzzy term expansion, and `AND` combination exactly, with the
//! upstream test table as the oracle.
//!
//! Two details decide that fidelity and are easy to lose:
//!
//! - **Term splitting is CJK-aware.** `searchTerms` NFKC-normalizes, then splits into runs, and a
//!   CJK run (Han, Hiragana, Katakana, Hangul, by *script extension*, so `ー` and `々` count) yields
//!   its overlapping character **pairs plus its last character** instead of a word. That is how
//!   `审` matches `自动代码审查` (as a prefix of the pair `审查`) and `查` matches (as the retained
//!   last character). Word-splitting in Rust would break every Chinese and Japanese query.
//! - **Distances are counted in UTF-16 code units**, because `MiniSearch` reads `term.length` and
//!   indexes the Levenshtein matrix by code unit. The fuzzy radius is therefore derived from the
//!   UTF-16 length, not the `char` count.
//!
//! NFKC itself is implemented here rather than pulled from a crate: the repository has no
//! `unicode-normalization` dependency, and the tables it needs (5,913 compatibility decompositions,
//! 1,104 primary composites, 393 combining-class ranges, the letter/mark/digit and CJK script
//! ranges) are generated from the Unicode 16.0.0 data that the JavaScript runtime itself uses, so
//! the normalization agrees character for character.
//!
//! # What this build already covered, and what the gap was
//!
//! `boot_gap.rs` (2,102 lines) took the *other* half of `packages/boot`: install specs, registry
//! plans, failure classification, `engines.dsh` admission. Its own module documentation states the
//! choice explicitly — "a Rust `MiniSearch` would not be `MiniSearch`" — and then does **not**
//! implement search, index validation, `normalizeIndexUrl`, or `marketInstallTarget`. A
//! symbol-by-symbol grep of the whole workspace before this file found zero hits for
//! `normalizeIndexUrl`, `parseIndexBody`, `parseMarketIndex`, `marketInstallTarget`,
//! `MarketSearch`, `searchTerms`, or any BM25/NFKC helper. `api_remotes_gap.rs` mentions
//! `plugin-market/changed`, but only as an event *name* in a table of Remote namespaces.
//!
//! So the largest remaining pure-function gap in `packages/boot` was this one: the search engine
//! (155 lines plus the semantics it borrows from `MiniSearch`), the format-1 validator (334), the
//! URL rule (26) and the install-target builder (~20).
//!
//! # What is implemented
//!
//! | Upstream symbol | Here |
//! | --- | --- |
//! | `searchTerms` (`search.ts`) | [`search_terms`] |
//! | the NFKC step of `searchTerms` | [`nfkc`], [`decompose`], [`canonical_order`], [`compose`] |
//! | `MarketSearch.select` | [`SearchIndex::select`] |
//! | `MarketSearch.build` (the `MiniSearch` index) | [`SearchIndex::new`] |
//! | `select`'s filters | [`MarketPluginMetadata`] matching in [`SearchIndex::select`] |
//! | `order` (`index`/`relevance`/`published`/`key`) | [`MarketSort`], [`SearchIndex::order`] |
//! | `MarketQuery`, `MarketSort` (`types.ts`) | [`MarketQuery`], [`MarketSort`] |
//! | the `offset`/`limit` checks in `PluginMarket.search` | [`validate_page`], [`MAX_LIMIT`] |
//! | `normalizeIndexUrl` (`index-url.ts`) | [`normalize_index_url`] |
//! | `marketInstallTarget` (`plugin-market-index`) | [`market_install_target`] |
//! | `MarketIndex` … `MarketVersion` (`types.ts`) | the record types below |
//! | `parseMarketIndex` (`plugin-market-index`) | [`parse_market_index`] |
//! | `parseIndexBody` (`parse.ts`) | [`parse_index_body`], [`IndexParseResult`] |
//!
//! # What is deliberately absent
//!
//! - Every byte of delegated I/O: `fetch`, the Worker Thread, the cache TTL map, the `git` partial
//!   clones and their LRU eviction, the GitLab files API and npm CDN reads, and `settings.update`.
//!   [`SearchIndex`] takes an already-validated [`MarketIndex`]; [`parse_index_body`] takes the
//!   complete response body. Who fetched it, and how, is the caller's business.
//! - `listingOf`/`installationOf`/`compatibilityOf` (`index.ts`). Those are pure, but they are
//!   profile-state projections — they read the running DSH version and the profile's bundle list —
//!   and `boot_gap.rs` already owns the admission half they depend on ([`crate`]'s
//!   `evaluate_plugin_compatibility` equivalent). Duplicating them here would split one decision
//!   across two modules.
//! - `repositoryOf`/`manifestRepository` (`detail.ts`, ~30 pure lines). They are `hosted-git-info`
//!   table lookups whose only local consumer would be the detail reader, which is entirely I/O.
//!
//! # Why every entry point takes its facts instead of reading them
//!
//! [`SearchIndex::new`] takes the index object, not a URL; [`normalize_index_url`] takes the string
//! a person typed, not the environment; [`parse_index_body`] takes the bytes, not a response. A
//! search that re-reads the world answers differently on a different machine and cannot be tested,
//! and this search decides what a person sees when they look for a plugin.
//!
//! # Why the crate root is spelled `girder_core`
//!
//! This module is not declared in `lib.rs` yet — wiring is a separate change. The test compiles
//! this exact source by `#[path]`, and under that inclusion `crate::` names the test binary, so
//! every path out of the module would have to go through the crate root's re-exports. Nothing here
//! needs one: the module imports only `std`.

#![allow(
    dead_code,
    reason = "this module is compiled privately by the `#[path]` test until lib.rs re-exports it; every item here is public surface for that wiring, so 'unused within this test binary' is not dead code"
)]

use std::cmp::Ordering;
use std::collections::{BTreeMap, BTreeSet};

// ============================================================================================
// Records — upstream `plugin-market-index/src/types.ts` and `plugin-market/src/types.ts`
// ============================================================================================

/// A localized string: either one string for every language, or one per language id — upstream
/// `LocalizedText`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum LocalizedText {
    /// One string, used for every language.
    Plain(String),
    /// One string per lowercase language id; format 1 requires `en`.
    Languages(BTreeMap<String, String>),
}

impl LocalizedText {
    /// Every language variant, in declaration order — upstream `variants()`.
    ///
    /// The order is by language id rather than JSON insertion order, which differs from JavaScript
    /// only in the order of the strings *inside a searchable field*; the field's term set, and so
    /// every score, is unaffected.
    #[must_use]
    pub fn variants(&self) -> Vec<&str> {
        match self {
            Self::Plain(text) => vec![text.as_str()],
            Self::Languages(text) => text.values().map(String::as_str).collect(),
        }
    }
}

impl From<&str> for LocalizedText {
    fn from(text: &str) -> Self {
        Self::Plain(text.to_owned())
    }
}

impl From<String> for LocalizedText {
    fn from(text: String) -> Self {
        Self::Plain(text)
    }
}

/// A market-declared category or label: the display title of an opaque id — upstream
/// `MarketDeclaration`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MarketDeclaration {
    /// The id's title in every language it declares.
    pub title: LocalizedText,
}

/// A market-declared sort key — upstream `MarketSortKeyDeclaration`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MarketSortKeyDeclaration {
    /// The key's title in every language it declares.
    pub title: LocalizedText,
    /// The direction a listing should start in.
    pub order: SortDirection,
}

/// The direction a sort runs in.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SortDirection {
    /// Smallest first.
    Asc,
    /// Largest first.
    Desc,
}

impl SortDirection {
    /// The multiplier that turns a value difference into a result order: `1` ascending, `-1`
    /// descending.
    const fn sign(self) -> f64 {
        match self {
            Self::Asc => 1.0,
            Self::Desc => -1.0,
        }
    }
}

/// The declarations an index defines; absent groups are empty — upstream `MarketIndexMetadata`.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct MarketIndexMetadata {
    /// Category id → declaration.
    pub categories: BTreeMap<String, MarketDeclaration>,
    /// Label id → declaration.
    pub labels: BTreeMap<String, MarketDeclaration>,
    /// Sort-key id → declaration.
    pub sort_keys: BTreeMap<String, MarketSortKeyDeclaration>,
}

/// One plugin's market-defined values — upstream `MarketPluginMetadata`.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct MarketPluginMetadata {
    /// The declared category id this plugin belongs to.
    pub category: Option<String>,
    /// Free-form keywords, conventionally from the package manifest's `keywords`.
    pub keywords: Vec<String>,
    /// Declared label ids; a plugin carries each at most once.
    pub labels: Vec<String>,
    /// Declared sort-key id → measured value. An unavailable value is absent; zero is retained.
    pub sort_keys: BTreeMap<String, f64>,
}

/// Where a plugin's source is published — upstream `MarketRepository`. Display only.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MarketRepository {
    /// Absolute http(s) URL of the repository's web page.
    pub url: String,
    /// Relative POSIX directory of the package inside the repository; absent means the root.
    pub directory: Option<String>,
}

/// Where one exact version installs from — upstream `MarketSource`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum MarketSource {
    /// A published npm version.
    Npm {
        /// The registry to ask first; absent means the client's configured registry.
        registry: Option<String>,
    },
    /// A commit in a Git repository.
    Git {
        /// Canonical https repository URL.
        url: String,
        /// Lowercase 40-hex-digit commit SHA.
        commit: String,
        /// Relative POSIX package directory; absent means the repository root.
        path: Option<String>,
    },
}

/// Runtime requirements of one version — upstream `MarketEngines`.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct MarketEngines {
    /// The `SemVer` **range** of DSH versions this version runs on; absent means any version.
    pub dsh: Option<String>,
}

/// One installable version — upstream `MarketVersion`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MarketVersion {
    /// The exact `SemVer` version, unique within its plugin.
    pub version: String,
    /// Publish time in Unix **seconds**.
    pub publish_timestamp: i64,
    /// Where the version installs from.
    pub source: MarketSource,
    /// The version's runtime requirements.
    pub engines: MarketEngines,
}

/// One installable bundle and its listed versions — upstream `MarketPlugin`.
#[derive(Clone, Debug, PartialEq)]
pub struct MarketPlugin {
    /// The npm package name, unique within its index.
    pub name: String,
    /// Localized display title.
    pub title: Option<LocalizedText>,
    /// Localized description.
    pub description: Option<LocalizedText>,
    /// A base64 `data:` URL or an absolute URL.
    pub icon: Option<String>,
    /// The author, as published.
    pub author: Option<String>,
    /// Absolute http(s) URL of a page about the plugin.
    pub homepage: Option<String>,
    /// The plugin's source repository.
    pub repository: Option<MarketRepository>,
    /// Category, keywords, labels, and sort-key values.
    pub metadata: MarketPluginMetadata,
    /// Non-empty, in descending `SemVer` precedence.
    pub versions: Vec<MarketVersion>,
}

/// One validated format-1 index — upstream `MarketIndex`.
#[derive(Clone, Debug, PartialEq)]
pub struct MarketIndex {
    /// The format version, always `1`.
    pub format: u8,
    /// The market's title.
    pub title: LocalizedText,
    /// The market's description.
    pub description: Option<LocalizedText>,
    /// The declarations plugins refer to.
    pub metadata: MarketIndexMetadata,
    /// Plugins in the index's own order. Positions in this vector are the ids search results carry.
    pub plugins: Vec<MarketPlugin>,
}

// ============================================================================================
// Query — upstream `MarketQuery` and `MarketSort`
// ============================================================================================

/// The largest page `search` returns — upstream `MAX_LIMIT`.
pub const MAX_LIMIT: usize = 100;

/// Order of search results; ties keep index order — upstream `MarketSort`.
///
/// `relevance` ranks by text match; without `text` every score is zero and the order is index order.
/// `published` uses each plugin's latest publish time. `key` uses a declared sort key, and plugins
/// without a value follow all others *in either direction*.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub enum MarketSort {
    /// The publisher's order.
    #[default]
    Index,
    /// Best text match first.
    Relevance,
    /// Latest publish time, in the given direction.
    Published {
        /// `Asc` for the oldest first.
        direction: SortDirection,
    },
    /// A declared sort key, in the given direction.
    Key {
        /// The declared sort-key id; an id the index does not declare sorts every plugin as
        /// valueless.
        id: String,
        /// `Asc` for the smallest first.
        direction: SortDirection,
    },
}

/// One page request over one index — upstream `MarketQuery`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MarketQuery {
    /// Fuzzy text matched against the name, author, keywords, and every language of the title,
    /// description, and category and label titles.
    pub text: Option<String>,
    /// The category id the plugin must have.
    pub category: Option<String>,
    /// The label ids the plugin must all have.
    pub labels: Vec<String>,
    /// The result order.
    pub sort: MarketSort,
    /// Matching plugins to skip.
    pub offset: usize,
    /// Largest number of plugins to return.
    pub limit: usize,
}

impl Default for MarketQuery {
    fn default() -> Self {
        Self {
            text: None,
            category: None,
            labels: Vec::new(),
            sort: MarketSort::Index,
            offset: 0,
            limit: MAX_LIMIT,
        }
    }
}

/// A page request field the wire format refused — upstream `plugin-market/invalid-query`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct QueryError {
    /// The offending query field.
    pub field: &'static str,
    /// The violated rule; upstream sends `range` for both fields.
    pub reason: &'static str,
}

impl std::fmt::Display for QueryError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{} must be in range", self.field)
    }
}

impl std::error::Error for QueryError {}

/// The largest integer JavaScript's `Number.isSafeInteger` accepts.
const MAX_SAFE_INTEGER: i64 = (1 << 53) - 1;

/// Validate a page request the way the Remote endpoint does before it fetches anything.
///
/// `offset` must be a safe integer of at least zero and `limit` an integer from 1 to
/// [`MAX_LIMIT`]; both are checked on the *wire* type, so a negative or fractional value is refused
/// here rather than silently clamped.
///
/// # Errors
///
/// [`QueryError`] naming `offset` or `limit`, both with reason `range`.
pub fn validate_page(offset: i64, limit: i64) -> Result<(usize, usize), QueryError> {
    if !(0..=MAX_SAFE_INTEGER).contains(&offset) {
        return Err(QueryError {
            field: "offset",
            reason: "range",
        });
    }
    if limit < 1 {
        return Err(QueryError {
            field: "limit",
            reason: "range",
        });
    }
    let limit = usize::try_from(limit).map_err(|_| QueryError {
        field: "limit",
        reason: "range",
    })?;
    if limit > MAX_LIMIT {
        return Err(QueryError {
            field: "limit",
            reason: "range",
        });
    }
    let offset = usize::try_from(offset).map_err(|_| QueryError {
        field: "offset",
        reason: "range",
    })?;
    Ok((offset, limit))
}

// ============================================================================================
// Unicode helpers — the NFKC step of `searchTerms`
// ============================================================================================

/// The canonical combining class of a character; zero for everything that is not a combining mark.
fn combining_class(ch: char) -> u8 {
    let cp = ch as u32;
    binary_range_with_value(CCC_RANGES, cp).unwrap_or(0)
}

/// Look a code point up in a `(start, end, value)` range table.
fn binary_range_with_value(table: &[(u32, u32, u8)], cp: u32) -> Option<u8> {
    let mut low = 0;
    let mut high = table.len();
    while low < high {
        let middle = low + (high - low) / 2;
        let (start, end, value) = table[middle];
        if cp < start {
            high = middle;
        } else if cp > end {
            low = middle + 1;
        } else {
            return Some(value);
        }
    }
    None
}

/// Whether a code point lies in any of a `(start, end)` range table.
fn in_ranges(table: &[(u32, u32)], cp: u32) -> bool {
    let mut low = 0;
    let mut high = table.len();
    while low < high {
        let middle = low + (high - low) / 2;
        let (start, end) = table[middle];
        if cp < start {
            high = middle;
        } else if cp > end {
            low = middle + 1;
        } else {
            return true;
        }
    }
    false
}

/// Whether `ch` is a letter, mark, or digit — the characters a search term is made of, and the
/// characters upstream's `RUN` alternation matches.
#[must_use]
pub fn is_word(ch: char) -> bool {
    in_ranges(WORD_RANGES, ch as u32)
}

/// Whether `ch` belongs to a CJK script (by script *extension*, so `ー` and `々` count) while being
/// a letter, mark, or digit.
#[must_use]
pub fn is_cjk(ch: char) -> bool {
    in_ranges(CJK_SCRIPT_RANGES, ch as u32)
}

// The Hangul constants of the Unicode Hangul syllable algorithm.
const HANGUL_S_BASE: u32 = 0xAC00;
const HANGUL_L_BASE: u32 = 0x1100;
const HANGUL_V_BASE: u32 = 0x1161;
const HANGUL_T_BASE: u32 = 0x11A7;
const HANGUL_L_COUNT: u32 = 19;
const HANGUL_V_COUNT: u32 = 21;
const HANGUL_T_COUNT: u32 = 28;
const HANGUL_S_COUNT: u32 = HANGUL_L_COUNT * HANGUL_V_COUNT * HANGUL_T_COUNT;

/// Append the full compatibility decomposition of one character, without canonical ordering.
fn decompose_into(ch: char, out: &mut Vec<char>) {
    let cp = ch as u32;
    if (HANGUL_S_BASE..HANGUL_S_BASE + HANGUL_S_COUNT).contains(&cp) {
        let index = cp - HANGUL_S_BASE;
        let lead = HANGUL_L_BASE + index / (HANGUL_V_COUNT * HANGUL_T_COUNT);
        let vowel = HANGUL_V_BASE + (index % (HANGUL_V_COUNT * HANGUL_T_COUNT)) / HANGUL_T_COUNT;
        let trailing = HANGUL_T_BASE + index % HANGUL_T_COUNT;
        out.push(char::from_u32(lead).unwrap_or(ch));
        out.push(char::from_u32(vowel).unwrap_or(ch));
        if trailing != HANGUL_T_BASE {
            out.push(char::from_u32(trailing).unwrap_or(ch));
        }
        return;
    }
    match nfkd_of(cp) {
        Some(expansion) => out.extend(expansion.chars()),
        None => out.push(ch),
    }
}

/// The fully expanded compatibility decomposition of a code point, if NFKD changes it.
fn nfkd_of(cp: u32) -> Option<&'static str> {
    let mut low = 0;
    let mut high = NFKD_TABLE.len();
    while low < high {
        let middle = low + (high - low) / 2;
        let (entry_cp, expansion) = NFKD_TABLE[middle];
        match cp.cmp(&entry_cp) {
            Ordering::Less => high = middle,
            Ordering::Greater => low = middle + 1,
            Ordering::Equal => return Some(expansion),
        }
    }
    None
}

/// The full compatibility decomposition of `text` (NFKD), before canonical ordering.
#[must_use]
pub fn decompose(text: &str) -> Vec<char> {
    let mut out = Vec::with_capacity(text.len());
    for ch in text.chars() {
        decompose_into(ch, &mut out);
    }
    out
}

/// Reorder combining marks into canonical order, in place.
///
/// A mark moves before the preceding marks with a strictly greater class and never past a starter;
/// marks of equal class keep their order.
pub fn canonical_order(chars: &mut [char]) {
    for index in 1..chars.len() {
        let class = combining_class(chars[index]);
        if class == 0 {
            continue;
        }
        let mut position = index;
        while position > 0 {
            let previous = combining_class(chars[position - 1]);
            if previous == 0 || previous <= class {
                break;
            }
            chars.swap(position - 1, position);
            position -= 1;
        }
    }
}

/// The primary composite of a starter and a combining mark, if one exists.
fn primary_composite(starter: char, mark: char) -> Option<char> {
    let start = starter as u32;
    let combine = mark as u32;
    // Hangul first: the syllable algorithm is arithmetic, so those pairs are not in the table.
    if (HANGUL_L_BASE..HANGUL_L_BASE + HANGUL_L_COUNT).contains(&start)
        && (HANGUL_V_BASE..HANGUL_V_BASE + HANGUL_V_COUNT).contains(&combine)
    {
        let index = (start - HANGUL_L_BASE) * HANGUL_V_COUNT + (combine - HANGUL_V_BASE);
        return char::from_u32(HANGUL_S_BASE + index * HANGUL_T_COUNT);
    }
    if (HANGUL_S_BASE..HANGUL_S_BASE + HANGUL_S_COUNT).contains(&start)
        && (start - HANGUL_S_BASE) % HANGUL_T_COUNT == 0
        && (HANGUL_T_BASE + 1..HANGUL_T_BASE + HANGUL_T_COUNT).contains(&combine)
    {
        return char::from_u32(start + (combine - HANGUL_T_BASE));
    }
    let mut low = 0;
    let mut high = COMPOSE_PAIRS.len();
    while low < high {
        let middle = low + (high - low) / 2;
        let (base, modifier, composed) = COMPOSE_PAIRS[middle];
        match (start, combine).cmp(&(base, modifier)) {
            Ordering::Less => high = middle,
            Ordering::Greater => low = middle + 1,
            Ordering::Equal => return char::from_u32(composed),
        }
    }
    None
}

/// Compose a canonically ordered decomposition back into the fewest characters (NFC).
///
/// A Hangul syllable takes a different path: the standard's composition algorithm forms an LV
/// syllable from a leading and a vowel jamo and then an LVT syllable from that plus a trailing
/// jamo, and those pairs are *not* "unblocked" the way a combining mark after a starter requires —
/// a trailing jamo has combining class zero, so the mark rule would never compose it. That is why
/// the syllable pair is tested before the blocking rule rather than inside it.
fn compose(chars: &[char]) -> String {
    let Some((&first, rest)) = chars.split_first() else {
        return String::new();
    };
    let mut out: Vec<char> = Vec::with_capacity(chars.len());
    out.push(first);
    let mut last_class = combining_class(first);
    let mut starter: Option<usize> = if last_class == 0 { Some(0) } else { None };
    for &ch in rest {
        let class = combining_class(ch);
        if let Some(position) = starter {
            if let Some(composed) = primary_composite(out[position], ch) {
                // A Hangul jamo composes even though it is a starter, so blocking is only tested
                // for the mark pairs.
                let unblocked = last_class == 0 || last_class < class;
                if unblocked || is_hangul_jamo(out[position], ch) {
                    out[position] = composed;
                    continue;
                }
            }
        }
        if class == 0 {
            starter = Some(out.len());
        }
        last_class = class;
        out.push(ch);
    }
    out.into_iter().collect()
}

/// Whether this pair is a Hangul jamo pair the syllable algorithm composes.
fn is_hangul_jamo(starter: char, next: char) -> bool {
    let start = starter as u32;
    let combine = next as u32;
    ((HANGUL_L_BASE..HANGUL_L_BASE + HANGUL_L_COUNT).contains(&start)
        && (HANGUL_V_BASE..HANGUL_V_BASE + HANGUL_V_COUNT).contains(&combine))
        || ((HANGUL_S_BASE..HANGUL_S_BASE + HANGUL_S_COUNT).contains(&start)
            && (start - HANGUL_S_BASE) % HANGUL_T_COUNT == 0
            && (HANGUL_T_BASE + 1..HANGUL_T_BASE + HANGUL_T_COUNT).contains(&combine))
}

/// Unicode NFKC normalization, matching `String.prototype.normalize('NFKC')`.
///
/// Compatibility decomposition, canonical ordering, then canonical composition — which is what
/// makes full-width `ＡＢＣ` match `abc`, `ﾊﾟｿｺﾝ` match `パソコン`, a decomposed `cafe\u{301}` match
/// `café`, and `㈱` match `株`.
#[must_use]
pub fn nfkc(text: &str) -> String {
    let mut decomposed = decompose(text);
    canonical_order(&mut decomposed);
    compose(&decomposed)
}

/// Split NFKC-normalized text into search terms — upstream `searchTerms`.
///
/// A run of letters, marks, and digits that is **not** CJK is one term. A CJK run yields its
/// overlapping character pairs followed by its last character, so a query matches without word
/// segmentation and every character of the run starts some term.
///
/// # Examples
///
/// ```
/// # use girder_core::plugin_market_gap::search_terms;
/// assert_eq!(search_terms("dsh-auto-review"), ["dsh", "auto", "review"]);
/// assert_eq!(search_terms("代码审查"), ["代码", "码审", "审查", "查"]);
/// assert_eq!(search_terms(" -- "), Vec::<String>::new());
/// ```
#[must_use]
pub fn search_terms(text: &str) -> Vec<String> {
    let normalized = nfkc(text);
    let chars: Vec<char> = normalized.chars().collect();
    let mut terms = Vec::new();
    let mut index = 0;
    while index < chars.len() {
        if !is_word(chars[index]) {
            index += 1;
            continue;
        }
        let cjk = is_cjk(chars[index]);
        let start = index;
        while index < chars.len() && is_word(chars[index]) && is_cjk(chars[index]) == cjk {
            index += 1;
        }
        if cjk {
            for position in start + 1..index {
                let pair: String = chars[position - 1..=position].iter().collect();
                terms.push(pair);
            }
            if let Some(&last) = chars.get(index - 1) {
                terms.push(last.to_string());
            }
        } else {
            terms.push(chars[start..index].iter().collect());
        }
    }
    terms
}

/// The length `String.prototype.length` reports: UTF-16 code units, which is what `MiniSearch`
/// measures a term and an edit distance with.
fn utf16_len(text: &str) -> usize {
    text.encode_utf16().count()
}

/// The Levenshtein distance between two terms in UTF-16 code units, or `None` beyond `limit`.
///
/// `MiniSearch` runs its matrix over code units, so a supplementary character costs two edits —
/// which is why this counts units rather than `char`s.
fn edit_distance(left: &str, right: &str, limit: usize) -> Option<usize> {
    let left: Vec<u16> = left.encode_utf16().collect();
    let right: Vec<u16> = right.encode_utf16().collect();
    if left.len().abs_diff(right.len()) > limit {
        return None;
    }
    let mut previous: Vec<usize> = (0..=right.len()).collect();
    let mut current = vec![0_usize; right.len() + 1];
    for (row, &left_unit) in left.iter().enumerate() {
        current[0] = row + 1;
        let mut row_minimum = current[0];
        for (column, &right_unit) in right.iter().enumerate() {
            let substitution = previous[column] + usize::from(left_unit != right_unit);
            let deletion = previous[column + 1] + 1;
            let insertion = current[column] + 1;
            let value = substitution.min(deletion).min(insertion);
            current[column + 1] = value;
            row_minimum = row_minimum.min(value);
        }
        if row_minimum > limit {
            return None;
        }
        std::mem::swap(&mut previous, &mut current);
    }
    let distance = previous[right.len()];
    (distance <= limit).then_some(distance)
}

// ============================================================================================
// The scoring parameters — `MiniSearch`'s, as `search.ts` configures them
// ============================================================================================

/// The number of searchable fields.
pub const FIELD_COUNT: usize = 6;

/// The searchable fields, in the field-id order `MiniSearch` assigns them — upstream `fields`.
pub const FIELD_NAMES: [&str; FIELD_COUNT] = [
    "name",
    "title",
    "keywords",
    "description",
    "author",
    "taxonomy",
];

/// Per-field score multipliers — upstream `searchOptions.boost`, with the fields it omits at 1.
const FIELD_BOOSTS: [f64; FIELD_COUNT] = [3.0, 3.0, 2.0, 1.0, 1.0, 1.0];

/// The weight of a term that extends the query term — upstream `weights.prefix` (`0.375`).
const PREFIX_WEIGHT: f64 = 0.375;

/// The weight of a term within the edit distance — upstream `weights.fuzzy` (`0.45`).
const FUZZY_WEIGHT: f64 = 0.45;

/// The largest edit distance any fuzzy match may have — upstream `searchOptions.maxFuzzy`.
const MAX_FUZZY: usize = 2;

/// The fraction of a term's length that may be edited — upstream `fuzzy: term => 0.2`.
const FUZZY_RATIO: f64 = 0.2;

/// The shortest term that is matched fuzzily at all — upstream `fuzzy: term => term.length >= 4`.
const FUZZY_MIN_LENGTH: usize = 4;

/// BM25's term-frequency saturation — upstream `defaultBM25params.k`.
const BM25_K: f64 = 1.2;

/// BM25's field-length normalization — upstream `defaultBM25params.b`.
const BM25_B: f64 = 0.7;

/// BM25's floor, which keeps a zero term frequency scoring above zero — upstream
/// `defaultBM25params.d`.
const BM25_D: f64 = 0.5;

/// The edit distance a term of this UTF-16 length may be matched within — upstream's
/// `Math.min(maxFuzzy, Math.round(term.length * 0.2))`, or no fuzzy matching below four characters.
fn fuzzy_radius(length: usize) -> Option<usize> {
    if length < FUZZY_MIN_LENGTH {
        return None;
    }
    #[allow(
        clippy::cast_precision_loss,
        clippy::cast_possible_truncation,
        clippy::cast_sign_loss,
        reason = "the radius is a small bounded count derived from a term length that cannot approach 2^53; the value is a rounded product of a length and a ratio, and it is clamped to MAX_FUZZY immediately"
    )]
    {
        let scaled = (length as f64 * FUZZY_RATIO).round();
        let scaled = scaled.max(0.0) as usize;
        let radius = scaled.min(MAX_FUZZY);
        (radius > 0).then_some(radius)
    }
}

/// One BM25 term score — upstream `calcBM25Score`.
///
/// `field_length` over `average_field_length` normalizes for length, `matching` over `total`
/// weights a term found in few documents, and `BM25_D` keeps a term that occurs in every document
/// from scoring zero.
fn bm25(
    term_frequency: u32,
    matching: usize,
    total: usize,
    field_length: u32,
    average: f64,
) -> f64 {
    #[allow(
        clippy::cast_precision_loss,
        reason = "a document count and a term frequency are counts held in memory, so they cannot approach 2^53"
    )]
    let frequency = f64::from(term_frequency);
    #[allow(
        clippy::cast_precision_loss,
        reason = "a document count is a count held in memory, so it cannot approach 2^53"
    )]
    let (matching, total) = (matching as f64, total as f64);
    #[allow(
        clippy::cast_precision_loss,
        reason = "a field length counts tokens held in memory, so it cannot approach 2^53"
    )]
    let field_length = f64::from(field_length);
    // An empty field is never indexed, so a zero average is unreachable; treating it as a zero
    // ratio rather than dividing keeps a degenerate table from producing NaN.
    let ratio = if average > 0.0 {
        field_length / average
    } else {
        0.0
    };
    let inverse_document_frequency = (1.0 + (total - matching + 0.5) / (matching + 0.5)).ln();
    inverse_document_frequency
        * (BM25_D
            + frequency * (BM25_K + 1.0) / (frequency + BM25_K * (1.0 - BM25_B + BM25_B * ratio)))
}

// ============================================================================================
// The search index — upstream `MarketSearch`
// ============================================================================================

/// Term → per-field postings: the documents holding the term and how often.
type Postings = BTreeMap<String, [BTreeMap<usize, u32>; FIELD_COUNT]>;

/// One query's page of results.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MarketPage {
    /// Positions in [`MarketIndex::plugins`] of the matching plugins on this page, in result order.
    pub positions: Vec<usize>,
    /// The number of matching plugins across every page.
    pub total: usize,
    /// The offset of the next page, or `None` after the last.
    pub next_offset: Option<usize>,
}

/// A built search index over one market index — upstream `MarketSearch`.
///
/// Building is the expensive half (`MiniSearch.addAll`); a caller that answers several queries keeps
/// one `SearchIndex` for the lifetime of the index document, which is what the service's per-document
/// cache does upstream.
#[derive(Clone, Debug)]
pub struct SearchIndex<'a> {
    index: &'a MarketIndex,
    postings: Postings,
    field_lengths: Vec<[u32; FIELD_COUNT]>,
    average_field_lengths: [f64; FIELD_COUNT],
}

impl<'a> SearchIndex<'a> {
    /// Build the index over every plugin of `index` — upstream `MarketSearch.build`.
    ///
    /// Each field is tokenized with [`search_terms`], lowercased, and counted. A field's length is
    /// its number of **distinct raw tokens** (before lowercasing), which is what `MiniSearch`
    /// records and what BM25's length normalization divides by.
    #[must_use]
    pub fn new(index: &'a MarketIndex) -> Self {
        let mut postings = Postings::new();
        let mut field_lengths = Vec::with_capacity(index.plugins.len());
        let mut totals = [0_f64; FIELD_COUNT];
        for (position, plugin) in index.plugins.iter().enumerate() {
            let fields = searchable_fields(plugin, &index.metadata);
            let mut lengths = [0_u32; FIELD_COUNT];
            for (field, value) in fields.iter().enumerate() {
                let tokens = search_terms(value);
                let distinct: BTreeSet<&String> = tokens.iter().collect();
                #[allow(
                    clippy::cast_possible_truncation,
                    reason = "a distinct-token count is bounded by the field's own byte length, so it cannot exceed a u32"
                )]
                let distinct_count = distinct.len() as u32;
                lengths[field] = distinct_count;
                totals[field] += f64::from(distinct_count);
                for token in &tokens {
                    let term = token.to_lowercase();
                    if term.is_empty() {
                        continue;
                    }
                    let entry = postings
                        .entry(term)
                        .or_insert_with(|| std::array::from_fn(|_| BTreeMap::new()));
                    *entry[field].entry(position).or_insert(0) += 1;
                }
            }
            field_lengths.push(lengths);
        }
        let count = index.plugins.len();
        let average_field_lengths = if count == 0 {
            [0.0; FIELD_COUNT]
        } else {
            #[allow(
                clippy::cast_precision_loss,
                reason = "a plugin count is a count held in memory, so it cannot approach 2^53"
            )]
            let count = count as f64;
            totals.map(|total| total / count)
        };
        Self {
            index,
            postings,
            field_lengths,
            average_field_lengths,
        }
    }

    /// The index this search was built over.
    #[must_use]
    pub const fn index(&self) -> &'a MarketIndex {
        self.index
    }

    /// Select, filter, and order the plugins matching a query, then take one page — upstream
    /// `MarketSearch.select` and the paging `PluginMarket.search` applies around it.
    ///
    /// # Errors
    ///
    /// [`QueryError`] when `limit` is zero or above [`MAX_LIMIT`]; the offset is a `usize` here, so
    /// its range is already guaranteed — use [`validate_page`] to check it as it arrives on the wire.
    pub fn select(&self, query: &MarketQuery) -> Result<MarketPage, QueryError> {
        if query.limit < 1 || query.limit > MAX_LIMIT {
            return Err(QueryError {
                field: "limit",
                reason: "range",
            });
        }
        let (mut positions, scores) = self.matching(query.text.as_deref().unwrap_or(""));
        positions.retain(|position| {
            let plugin = &self.index.plugins[*position];
            let category = query
                .category
                .as_ref()
                .is_none_or(|wanted| plugin.metadata.category.as_ref() == Some(wanted));
            let labels = query
                .labels
                .iter()
                .all(|label| plugin.metadata.labels.contains(label));
            category && labels
        });
        self.order(&mut positions, &query.sort, &scores);
        let total = positions.len();
        let start = query.offset.min(total);
        let end = start.saturating_add(query.limit).min(total);
        let next = end;
        Ok(MarketPage {
            positions: positions[start..end].to_vec(),
            total,
            next_offset: (next < total).then_some(next),
        })
    }

    /// Every plugin position matching `text`, with its relevance score — the `MiniSearch.search` half
    /// of [`Self::select`], exposed because a caller that ranks by relevance needs the scores and
    /// because they are what the upstream test table pins down.
    ///
    /// An empty or wordless `text` matches every plugin with no score at all, which
    /// [`MarketSort::Relevance`] then leaves in index order.
    #[must_use]
    pub fn scores(&self, text: &str) -> BTreeMap<usize, f64> {
        self.matching(text).1
    }

    /// The positions that match `text` and their scores — the `MiniSearch.search` half of `select`.
    fn matching(&self, text: &str) -> (Vec<usize>, BTreeMap<usize, f64>) {
        let terms: Vec<String> = search_terms(text)
            .iter()
            .map(|term| term.to_lowercase())
            .filter(|term| !term.is_empty())
            .collect();
        if terms.is_empty() {
            return ((0..self.index.plugins.len()).collect(), BTreeMap::new());
        }
        let distinct_terms = {
            let mut distinct: BTreeSet<&String> = BTreeSet::new();
            for term in &terms {
                distinct.insert(term);
            }
            distinct.len()
        };
        let mut combined: Option<BTreeMap<usize, f64>> = None;
        for term in &terms {
            let scores = self.term_scores(term);
            combined = Some(match combined {
                None => scores,
                Some(previous) => previous
                    .into_iter()
                    .filter_map(|(position, score)| {
                        scores.get(&position).map(|added| (position, score + added))
                    })
                    .collect(),
            });
        }
        // `MiniSearch.search` scales a matched document's score by the number of distinct query
        // terms that matched it (`quality = terms.length || 1`). Under `combineWith: 'AND'` every
        // surviving document matched them all, so the factor is the query's own distinct term count.
        #[allow(
            clippy::cast_precision_loss,
            reason = "a query's distinct term count is bounded by the query text, so it cannot approach 2^53"
        )]
        let quality = combined.as_ref().map_or(1.0, |_| distinct_terms as f64);
        let scores = combined
            .unwrap_or_default()
            .into_iter()
            .map(|(position, score)| (position, score * quality))
            .collect::<BTreeMap<usize, f64>>();
        let positions = scores.keys().copied().collect();
        (positions, scores)
    }

    /// Every document score for one query term — upstream `executeQuerySpec` plus `termResults`.
    fn term_scores(&self, term: &str) -> BTreeMap<usize, f64> {
        let mut scores = BTreeMap::new();
        // 1. The term exactly as written.
        self.add_term_scores(term, 1.0, &mut scores);
        // 2. Indexed terms this term prefixes, minus the exact match.
        let prefix_length = utf16_len(term);
        let mut prefixed: BTreeSet<String> = BTreeSet::new();
        for candidate in self.postings.range(term.to_owned()..) {
            if !candidate.0.starts_with(term) {
                break;
            }
            if candidate.0 == term {
                continue;
            }
            prefixed.insert(candidate.0.clone());
            let length = utf16_len(candidate.0);
            let distance = length - prefix_length;
            #[allow(
                clippy::cast_precision_loss,
                reason = "a term length counts UTF-16 units held in memory, so it cannot approach 2^53"
            )]
            let weight = PREFIX_WEIGHT * length as f64 / (length as f64 + 0.3 * distance as f64);
            self.add_term_scores(candidate.0, weight, &mut scores);
        }
        // 3. Indexed terms within the edit distance, minus those already scored as prefixes.
        if let Some(radius) = fuzzy_radius(prefix_length) {
            for candidate in self.postings.keys() {
                if prefixed.contains(candidate) {
                    continue;
                }
                let Some(distance) = edit_distance(candidate, term, radius) else {
                    continue;
                };
                // Distance zero is the exact match, already scored.
                if distance == 0 {
                    continue;
                }
                let length = utf16_len(candidate);
                #[allow(
                    clippy::cast_precision_loss,
                    reason = "a term length counts UTF-16 units held in memory, so it cannot approach 2^53"
                )]
                let weight = FUZZY_WEIGHT * length as f64 / (length as f64 + distance as f64);
                self.add_term_scores(candidate, weight, &mut scores);
            }
        }
        scores
    }

    /// Add one derived term's BM25 contribution to every document holding it — upstream
    /// `termResults`.
    fn add_term_scores(&self, derived: &str, weight: f64, scores: &mut BTreeMap<usize, f64>) {
        let Some(fields) = self.postings.get(derived) else {
            return;
        };
        let total = self.index.plugins.len();
        for (field, postings) in fields.iter().enumerate() {
            let boost = FIELD_BOOSTS[field];
            let matching = postings.len();
            let average = self.average_field_lengths[field];
            for (&position, &frequency) in postings {
                let field_length = self.field_lengths[position][field];
                let raw = bm25(frequency, matching, total, field_length, average);
                *scores.entry(position).or_insert(0.0) += weight * boost * raw;
            }
        }
    }

    /// Put the matches in the requested order — upstream `MarketSearch.order`.
    fn order(&self, positions: &mut [usize], sort: &MarketSort, scores: &BTreeMap<usize, f64>) {
        match sort {
            MarketSort::Index => {}
            MarketSort::Relevance => positions.sort_by(|left, right| {
                // Descending score, then index order.
                scores
                    .get(right)
                    .copied()
                    .unwrap_or(0.0)
                    .partial_cmp(&scores.get(left).copied().unwrap_or(0.0))
                    .unwrap_or(Ordering::Equal)
                    .then_with(|| left.cmp(right))
            }),
            MarketSort::Published { direction } => {
                let sign = direction.sign();
                positions.sort_by(|left, right| {
                    compare_optional(
                        self.published_at(*left),
                        self.published_at(*right),
                        sign,
                        *left,
                        *right,
                    )
                });
            }
            MarketSort::Key { id, direction } => {
                let sign = direction.sign();
                positions.sort_by(|left, right| {
                    compare_optional(
                        self.sort_key(*left, id),
                        self.sort_key(*right, id),
                        sign,
                        *left,
                        *right,
                    )
                });
            }
        }
    }

    /// A plugin's latest publish time — upstream's `Math.max(...versions.map(...))`.
    ///
    /// A plugin with no versions has no value; format 1 guarantees that cannot happen.
    fn published_at(&self, position: usize) -> Option<f64> {
        self.index.plugins[position]
            .versions
            .iter()
            .map(|version| {
                #[allow(
                    clippy::cast_precision_loss,
                    reason = "a publish timestamp is a Unix second count whose exact f64 range is far beyond any real date; ordering two of them is all this value is used for"
                )]
                let seconds = version.publish_timestamp as f64;
                seconds
            })
            .reduce(f64::max)
    }

    /// A plugin's declared sort-key value, or `None` when it has none for `id`.
    fn sort_key(&self, position: usize, id: &str) -> Option<f64> {
        self.index.plugins[position]
            .metadata
            .sort_keys
            .get(id)
            .copied()
    }
}

/// Order two optional sort values, placing valueless plugins after every valued one in either
/// direction — upstream's `published`/`key` comparator.
///
/// Ties, and a comparison that produces `NaN`, fall back to index order, which is what
/// JavaScript's `|| a - b` does.
fn compare_optional(
    left: Option<f64>,
    right: Option<f64>,
    sign: f64,
    left_position: usize,
    right_position: usize,
) -> Ordering {
    match (left, right) {
        (None, None) => left_position.cmp(&right_position),
        (None, Some(_)) => Ordering::Greater,
        (Some(_), None) => Ordering::Less,
        (Some(left), Some(right)) => match ((left - right) * sign).partial_cmp(&0.0) {
            Some(Ordering::Less) => Ordering::Less,
            Some(Ordering::Greater) => Ordering::Greater,
            _ => left_position.cmp(&right_position),
        },
    }
}

/// The six searchable strings of one plugin — upstream's `SearchDocument` construction.
///
/// Every field is a string (possibly empty), so every document has a length for every field and the
/// per-field average is a plain mean over all plugins.
fn searchable_fields(
    plugin: &MarketPlugin,
    metadata: &MarketIndexMetadata,
) -> [String; FIELD_COUNT] {
    let title = plugin
        .title
        .as_ref()
        .map(|text| text.variants().join("\n"))
        .unwrap_or_default();
    let description = plugin
        .description
        .as_ref()
        .map(|text| text.variants().join("\n"))
        .unwrap_or_default();
    let mut taxonomy: Vec<&str> = Vec::new();
    if let Some(category) = &plugin.metadata.category
        && let Some(declaration) = metadata.categories.get(category)
    {
        taxonomy.extend(declaration.title.variants());
    }
    for label in &plugin.metadata.labels {
        if let Some(declaration) = metadata.labels.get(label) {
            taxonomy.extend(declaration.title.variants());
        }
    }
    [
        plugin.name.clone(),
        title,
        plugin.metadata.keywords.join("\n"),
        description,
        plugin.author.clone().unwrap_or_default(),
        taxonomy.join("\n"),
    ]
}

// ============================================================================================
// The index-source URL rule — upstream `index-url.ts`
// ============================================================================================

/// A parsed absolute http(s) URL: the subset of WHATWG URL this module needs.
///
/// The subset is deliberate. A search result's scores never depend on a URL, and the only rules here
/// compare a URL's serialization to the text a person wrote, so what is needed is a parse that
/// canonicalizes the parts WHATWG canonicalizes (scheme and host case, the default port, the root
/// path, `.`/`..` segments) and preserves the empty `?`/`#` markers the rules test for.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Url {
    /// Lowercase scheme, without the colon.
    pub scheme: String,
    /// User information before `@`, or empty.
    pub username: String,
    /// Password after `:`, or empty.
    pub password: String,
    /// Lowercase host.
    pub host: String,
    /// The port, when it is not the scheme's default.
    pub port: Option<u16>,
    /// Absolute path, always starting with `/`.
    pub path: String,
    /// The query, without `?`; `None` when there was no `?` at all.
    pub query: Option<String>,
    /// The fragment, without `#`; `None` when there was no `#` at all.
    pub fragment: Option<String>,
}

impl Url {
    /// Parse an absolute http(s) URL, or something that is not one.
    ///
    /// Returns `None` for text with no scheme, no authority, a non-http(s) scheme, an unparsable
    /// port, or characters WHATWG would percent-encode — the last is a documented limit of this
    /// subset: a non-ASCII URL is refused rather than silently rewritten.
    #[must_use]
    pub fn parse(text: &str) -> Option<Self> {
        let (before_fragment, fragment) = match text.split_once('#') {
            Some((head, rest)) => (head, Some(rest)),
            None => (text, None),
        };
        let (body, query) = match before_fragment.split_once('?') {
            Some((head, rest)) => (head, Some(rest)),
            None => (before_fragment, None),
        };
        let (scheme, rest) = body.split_once(':')?;
        if scheme.is_empty() || !scheme.starts_with(|ch: char| ch.is_ascii_alphabetic()) {
            return None;
        }
        if !scheme
            .chars()
            .all(|ch| ch.is_ascii_alphanumeric() || matches!(ch, '+' | '-' | '.'))
        {
            return None;
        }
        let scheme = scheme.to_ascii_lowercase();
        // A scheme-relative URL such as `javascript:alert(1)` has no authority; it is still a URL,
        // and refusing it here would report "not absolute" where the rule to apply is the scheme
        // rule. WHATWG calls this an opaque path, and it never equals a http(s) origin.
        let Some(rest) = rest.strip_prefix("//") else {
            return Some(Self {
                scheme,
                username: String::new(),
                password: String::new(),
                host: String::new(),
                port: None,
                path: rest.to_owned(),
                query: query.map(str::to_owned),
                fragment: fragment.map(str::to_owned),
            });
        };
        let authority_end = rest.find(['/', '?', '#']).unwrap_or(rest.len());
        let (authority, path_and_rest) = rest.split_at(authority_end);
        let (userinfo, hostport) = match authority.rsplit_once('@') {
            Some((userinfo, hostport)) => (Some(userinfo), hostport),
            None => (None, authority),
        };
        let (username, password) = match userinfo {
            Some(userinfo) => match userinfo.split_once(':') {
                Some((username, password)) => (username.to_owned(), password.to_owned()),
                None => (userinfo.to_owned(), String::new()),
            },
            None => (String::new(), String::new()),
        };
        let (host, port) = split_host_port(hostport)?;
        // A non-special scheme may have an empty authority — `file:///srv/x.json` is a valid URL
        // whose origin is the string `"null"`. A http(s) URL may not.
        let special = scheme == "http" || scheme == "https";
        if special && host.is_empty() {
            return None;
        }
        // An empty host is allowed only for a non-special scheme, where it is the `null` origin.
        if (special || !host.is_empty()) && !is_host(host) {
            return None;
        }
        let path = normalize_path(path_and_rest);
        Some(Self {
            scheme,
            username,
            password,
            host: host.to_ascii_lowercase(),
            port,
            path,
            query: query.map(str::to_owned),
            fragment: fragment.map(str::to_owned),
        })
    }

    /// The default port of this scheme.
    fn default_port(&self) -> u16 {
        if self.is_http() { 80 } else { 443 }
    }

    /// Whether this URL uses the `http` scheme.
    ///
    /// The scheme is lowercased at construction, so its length decides: `http` is four bytes and
    /// `https` is five.
    fn is_http(&self) -> bool {
        // The scheme is lowercased at construction, so `http` differs from `https` in length alone.
        self.scheme == "http"
    }

    /// This URL's serialization — what WHATWG's `href` returns for the same parts.
    #[must_use]
    pub fn href(&self) -> String {
        let mut out = String::with_capacity(self.path.len() + 32);
        out.push_str(&self.scheme);
        out.push_str("://");
        if !self.username.is_empty() || !self.password.is_empty() {
            out.push_str(&self.username);
            if !self.password.is_empty() {
                out.push(':');
                out.push_str(&self.password);
            }
            out.push('@');
        }
        out.push_str(&self.host);
        if let Some(port) = self.port.filter(|port| *port != self.default_port()) {
            out.push(':');
            out.push_str(&port.to_string());
        }
        out.push_str(&self.path);
        if let Some(query) = &self.query {
            out.push('?');
            out.push_str(query);
        }
        if let Some(fragment) = &self.fragment {
            out.push('#');
            out.push_str(fragment);
        }
        out
    }

    /// The origin, `scheme://host[:port]` with the default port omitted, or `None` for a scheme
    /// that has none.
    ///
    /// WHATWG gives a non-special scheme the serialized origin `"null"`, which never equals a
    /// http(s) origin — that is what makes a `file:` index refuse a resolved `file:` icon instead
    /// of accepting it as same-origin.
    #[must_use]
    pub fn origin(&self) -> Option<String> {
        if self.scheme != "http" && self.scheme != "https" {
            return None;
        }
        let mut out = String::with_capacity(self.host.len() + 12);
        out.push_str(&self.scheme);
        out.push_str("://");
        out.push_str(&self.host);
        if let Some(port) = self.port.filter(|port| *port != self.default_port()) {
            out.push(':');
            out.push_str(&port.to_string());
        }
        Some(out)
    }
}

/// Whether `text` is a host a URL may carry: an IPv6 literal in balanced brackets, or a run of
/// letters, digits, dots, hyphens, and underscores.
///
/// The bracket balance is what makes `https://[` unparsable rather than a host named `[`, which is
/// the difference between "must be a URL reference" and "must resolve to https or the index origin".
fn is_host(text: &str) -> bool {
    if let Some(rest) = text.strip_prefix('[') {
        let Some((inside, after)) = rest.split_once(']') else {
            return false;
        };
        return after.is_empty()
            && !inside.is_empty()
            && inside
                .chars()
                .all(|ch| ch.is_ascii_hexdigit() || matches!(ch, ':' | '.'));
    }
    !text.is_empty()
        && text
            .chars()
            .all(|ch| ch.is_ascii_alphanumeric() || matches!(ch, '.' | '-' | '_'))
}

/// Split `host[:port]`, refusing a malformed port.
fn split_host_port(text: &str) -> Option<(&str, Option<u16>)> {
    // A bracketed IPv6 literal may contain colons of its own.
    let (host, port) = if let Some(close) = text.find(']') {
        if text.starts_with('[') {
            (&text[..=close], text.get(close + 1..).unwrap_or(""))
        } else {
            (text, "")
        }
    } else {
        match text.rsplit_once(':') {
            Some((host, _port)) => (host, &text[host.len()..]),
            None => (text, ""),
        }
    };
    let port = match port.strip_prefix(':') {
        Some(digits) => {
            if digits.is_empty() || !digits.bytes().all(|byte| byte.is_ascii_digit()) {
                return None;
            }
            Some(digits.parse::<u16>().ok()?)
        }
        None if port.is_empty() => None,
        None => return None,
    };
    Some((host, port))
}

/// Serialize a path, collapsing `.` and `..` segments as WHATWG does and ensuring a leading `/`.
fn normalize_path(path: &str) -> String {
    let mut segments: Vec<&str> = Vec::new();
    let trailing_slash = path.ends_with('/');
    for segment in path.split('/') {
        match segment {
            "" | "." => {}
            ".." => {
                segments.pop();
            }
            other => segments.push(other),
        }
    }
    let mut out = String::with_capacity(path.len() + 1);
    out.push('/');
    out.push_str(&segments.join("/"));
    if trailing_slash && !segments.is_empty() {
        out.push('/');
    }
    out
}

/// Why an index-source URL was refused — upstream's `plugin-market/invalid-index-url` reasons.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum IndexUrlReason {
    /// The text is not an absolute URL.
    NotAbsolute,
    /// The scheme is not `http` or `https`.
    Scheme,
    /// The URL carries credentials or a fragment.
    CredentialsOrFragment,
}

impl IndexUrlReason {
    /// The reason as upstream sends it in the error details.
    #[must_use]
    pub const fn label(self) -> &'static str {
        match self {
            Self::NotAbsolute => "not-absolute",
            Self::Scheme => "scheme",
            Self::CredentialsOrFragment => "credentials-or-fragment",
        }
    }
}

/// An index-source URL that was refused.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct InvalidIndexUrl {
    /// The URL as written.
    pub url: String,
    /// Which rule it violated.
    pub reason: IndexUrlReason,
    /// The message upstream reports, which names the rule rather than the parser's complaint.
    pub message: String,
}

impl std::fmt::Display for InvalidIndexUrl {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.message)
    }
}

impl std::error::Error for InvalidIndexUrl {}

/// Validate and normalize one index source URL — upstream `normalizeIndexUrl`.
///
/// Only http(s) URLs are accepted, and they carry no credentials and no fragment: an index source
/// is fetched by machine, and a URL with either would send a secret to a log or fetch something
/// other than what it names.
///
/// # Errors
///
/// [`InvalidIndexUrl`] with reason `not-absolute` for unparseable text, `scheme` for anything but
/// http(s), and `credentials-or-fragment` for user information or a `#`.
pub fn normalize_index_url(text: &str) -> Result<String, InvalidIndexUrl> {
    let refuse = |reason: IndexUrlReason, message: String| InvalidIndexUrl {
        url: text.to_owned(),
        reason,
        message,
    };
    let Some(url) = Url::parse(text.trim()) else {
        return Err(refuse(
            IndexUrlReason::NotAbsolute,
            format!("Not an absolute URL: {text}"),
        ));
    };
    if url.scheme != "http" && url.scheme != "https" {
        return Err(refuse(
            IndexUrlReason::Scheme,
            format!("Index URLs use http or https: {text}"),
        ));
    }
    if !url.username.is_empty() || !url.password.is_empty() || url.fragment.is_some() {
        return Err(refuse(
            IndexUrlReason::CredentialsOrFragment,
            format!("Index URLs carry no credentials or fragment: {text}"),
        ));
    }
    Ok(url.href())
}

// ============================================================================================
// Install arguments — upstream `marketInstallTarget`
// ============================================================================================

/// The arguments that install one exact version — upstream `MarketInstallTarget`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MarketInstallTarget {
    /// The install spec `pnpm add` accepts.
    pub spec: String,
    /// The registry to ask first; absent means the client's configured one.
    pub registry: Option<String>,
}

/// Build the arguments that install one exact version — upstream `marketInstallTarget`.
///
/// An npm version pins `name@version`, keeping the registry that version names. A Git version pins
/// the commit in a `git+https` URL — adding the `.git` suffix pnpm expects — and appends pnpm's
/// `&path:` parameter when the package is a directory inside the repository.
///
/// # Examples
///
/// ```
/// # use girder_core::plugin_market_gap::{MarketSource, MarketVersion, MarketEngines, market_install_target};
/// let version = MarketVersion {
///     version: "1.2.3".to_owned(),
///     publish_timestamp: 0,
///     source: MarketSource::Npm { registry: None },
///     engines: MarketEngines::default(),
/// };
/// assert_eq!(market_install_target("dsh-foo", &version).spec, "dsh-foo@1.2.3");
/// ```
#[must_use]
pub fn market_install_target(name: &str, version: &MarketVersion) -> MarketInstallTarget {
    match &version.source {
        MarketSource::Npm { registry } => MarketInstallTarget {
            spec: format!("{name}@{}", version.version),
            registry: registry.clone(),
        },
        MarketSource::Git { url, commit, path } => {
            #[allow(
                clippy::case_sensitive_file_extension_comparisons,
                reason = "a Git remote URL's `.git` suffix is case-sensitive in practice, and a case-insensitive match would rewrite a URL that has none"
            )]
            let repository = if url.ends_with(".git") {
                url.clone()
            } else {
                format!("{url}.git")
            };
            let suffix = match path {
                Some(path) => format!("&path:/{path}"),
                None => String::new(),
            };
            MarketInstallTarget {
                spec: format!("git+{repository}#{commit}{suffix}"),
                registry: None,
            }
        }
    }
}

// ============================================================================================
// Generated Unicode tables
// ============================================================================================

// ============================================================================================
// Format-1 index validation — upstream `plugin-market-index/src/index.ts`
// ============================================================================================

/// The largest decoded payload of an inline `data:` icon — upstream `MAX_INLINE_ICON_BYTES`.
pub const MAX_INLINE_ICON_BYTES: usize = 32 * 1024;

/// The largest npm package name npm accepts — upstream `PACKAGE_NAME_MAX_LENGTH`.
const PACKAGE_NAME_MAX_LENGTH: usize = 214;

/// The exclusive upper bound of a publish timestamp, in Unix seconds — upstream
/// `MAX_PUBLISH_TIMESTAMP`.
const MAX_PUBLISH_TIMESTAMP: f64 = 1e11;

/// An index that violates format 1 — upstream `MarketIndexError`.
///
/// `path` is the JSON path of the offending value, such as
/// `$.plugins[0].versions[1].version`, which is what makes the diagnostic usable on a 100,000-line
/// document.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MarketIndexError {
    /// JSON path of the offending value.
    pub path: String,
    /// The violated rule.
    pub reason: String,
}

impl std::fmt::Display for MarketIndexError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{} {}", self.path, self.reason)
    }
}

impl std::error::Error for MarketIndexError {}

/// Build a violation, returning it — upstream `fail`.
fn fail<T>(path: &str, reason: impl Into<String>) -> Result<T, MarketIndexError> {
    Err(MarketIndexError {
        path: path.to_owned(),
        reason: reason.into(),
    })
}

/// One JSON object, viewed for member access — upstream `Json`.
type Json = serde_json::Map<String, serde_json::Value>;

/// Require an object, without arrays and without null — upstream `objectAt`.
fn object_at<'v>(value: &'v serde_json::Value, path: &str) -> Result<&'v Json, MarketIndexError> {
    match value {
        serde_json::Value::Object(map) => Ok(map),
        _ => fail(path, "must be an object"),
    }
}

/// Require a string with a non-whitespace character — upstream `stringAt`.
fn string_at<'v>(value: &'v serde_json::Value, path: &str) -> Result<&'v str, MarketIndexError> {
    match value {
        serde_json::Value::String(text) if !text.trim().is_empty() => Ok(text),
        _ => fail(path, "must be a string with a non-whitespace character"),
    }
}

/// Read an optional member; a null member counts as absent — upstream `optional` — except where the
/// caller tests for null itself, which [`plugin_metadata`] does.
fn member<'v>(object: &'v Json, key: &str) -> Option<&'v serde_json::Value> {
    object.get(key).filter(|value| !value.is_null())
}

/// Read a required-or-optional member through `read`, prefixing the path — upstream
/// `optional(object, key, path, read)`.
fn optional<T>(
    object: &Json,
    key: &str,
    path: &str,
    read: impl FnOnce(&serde_json::Value, &str) -> Result<T, MarketIndexError>,
) -> Result<Option<T>, MarketIndexError> {
    match member(object, key) {
        Some(value) => read(value, &format!("{path}.{key}")).map(Some),
        None => Ok(None),
    }
}

/// Whether `text` is a lowercase language id — upstream `LANGUAGE_ID`.
fn is_language_id(text: &str) -> bool {
    let mut parts = text.split('-');
    let Some(first) = parts.next() else {
        return false;
    };
    if !(2..=8).contains(&first.len()) || !first.bytes().all(|byte| byte.is_ascii_lowercase()) {
        return false;
    }
    parts.all(|part| {
        (1..=8).contains(&part.len())
            && part
                .bytes()
                .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit())
    })
}

/// Whether `text` is an npm package name — upstream `PACKAGE_NAME`.
fn is_package_name(text: &str) -> bool {
    let allowed = |text: &str| {
        let mut bytes = text.bytes();
        let Some(first) = bytes.next() else {
            return false;
        };
        if !first.is_ascii_lowercase() && !first.is_ascii_digit() {
            return false;
        }
        bytes.all(|byte| {
            byte.is_ascii_lowercase()
                || byte.is_ascii_digit()
                || matches!(byte, b'.' | b'_' | b'~' | b'-')
        })
    };
    match text.split_once('/') {
        Some((scope, name)) => {
            let Some(scope) = scope.strip_prefix('@') else {
                return false;
            };
            allowed(scope) && allowed(name) && !name.contains('/')
        }
        None => allowed(text),
    }
}

/// Whether `byte` may appear in a `SemVer` numeric or prerelease identifier — upstream `SEMVER`'s
/// character classes.
fn is_semver_part(text: &str) -> bool {
    !text.is_empty()
        && text
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-')
}

/// Whether `text` is one dotted run whose identifiers are all valid — the tail of a `SemVer` version.
fn is_identifier_run(text: &str) -> bool {
    text.split('.').all(is_semver_part)
}

/// Whether `text` is an exact `SemVer` 2.0.0 version — upstream `SEMVER`.
///
/// This is the *grammar*, so it refuses `v1.2.3`, `^1.2.3`, `1.2.3beta`, and a numeric field with a
/// leading zero, all of which the format refuses.
fn is_exact_semver(text: &str) -> bool {
    let (rest, build) = match text.split_once('+') {
        Some((rest, build)) => (rest, Some(build)),
        None => (text, None),
    };
    if let Some(build) = build
        && !is_identifier_run(build)
    {
        return false;
    }
    let (core, prerelease) = match rest.split_once('-') {
        Some((core, prerelease)) => (core, Some(prerelease)),
        None => (rest, None),
    };
    if let Some(prerelease) = prerelease
        && !is_identifier_run(prerelease)
    {
        return false;
    }
    let mut fields = core.split('.');
    let (Some(major), Some(minor), Some(patch)) = (fields.next(), fields.next(), fields.next())
    else {
        return false;
    };
    if fields.next().is_some() {
        return false;
    }
    [major, minor, patch].into_iter().all(|field| {
        !field.is_empty()
            && field.bytes().all(|byte| byte.is_ascii_digit())
            && (field.len() == 1 || !field.starts_with('0'))
    })
}

/// Whether `text` is a 40-hex-digit lowercase commit — upstream `COMMIT`.
fn is_commit(text: &str) -> bool {
    text.len() == 40
        && text
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
}

/// Whether `segment` is one `PATH_SEGMENT` of a relative package directory.
fn is_path_segment(segment: &str) -> bool {
    !segment.is_empty()
        && segment != "."
        && segment != ".."
        && segment.bytes().all(|byte| {
            byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'_' | b'@' | b'+' | b'-')
        })
}

/// Whether `text` is a relative POSIX directory of allowed segments — upstream `packagePath`.
fn is_package_path(text: &str) -> bool {
    text.split('/').all(is_path_segment)
}

/// Whether `text` is base64 with correct padding — upstream `BASE64`.
///
/// The shape is `(group of four base64 characters)*` followed by at most one group whose padding
/// matches the two or three characters it completes.
fn is_base64(text: &str) -> bool {
    let bytes = text.as_bytes();
    if bytes.len() % 4 != 0 {
        return false;
    }
    let padding = bytes.iter().rev().take_while(|byte| **byte == b'=').count();
    if padding > 2 {
        return false;
    }
    let body = &bytes[..bytes.len() - padding];
    if !body
        .iter()
        .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'+' | b'/'))
    {
        return false;
    }
    if padding == 0 {
        return true;
    }
    // The group the padding completes must carry exactly four characters before the padding.
    let group_length = body.len() % 4;
    group_length + padding == 4
}

/// A localized string — upstream `localizedText`.
///
/// A plain string is used for every language; an object must be keyed by lowercase language ids and
/// must declare `en`, because a market with no English title has nothing to show a fallback reader.
fn localized_text(
    value: &serde_json::Value,
    path: &str,
) -> Result<LocalizedText, MarketIndexError> {
    if matches!(value, serde_json::Value::String(_)) {
        return string_at(value, path).map(|text| LocalizedText::Plain(text.to_owned()));
    }
    let object = object_at(value, path)?;
    let mut languages = BTreeMap::new();
    for (language, member) in object {
        if !is_language_id(language) {
            return fail(
                &format!("{path}.{language}"),
                "must be keyed by a lowercase language id",
            );
        }
        let text = string_at(member, &format!("{path}.{language}"))?;
        languages.insert(language.clone(), text.to_owned());
    }
    if !languages.contains_key("en") {
        return fail(path, "must contain en");
    }
    Ok(LocalizedText::Languages(languages))
}

/// Read a map of declared ids — upstream `declarations`.
fn declarations<T>(
    value: &serde_json::Value,
    path: &str,
    read: impl Fn(&Json, &str) -> Result<T, MarketIndexError>,
) -> Result<BTreeMap<String, T>, MarketIndexError> {
    let object = object_at(value, path)?;
    let mut result = BTreeMap::new();
    for (id, member) in object {
        if id.is_empty() {
            return fail(path, "must not declare an empty id");
        }
        let at = format!("{path}.{id}");
        let parsed = read(object_at(member, &at)?, &at)?;
        result.insert(id.clone(), parsed);
    }
    Ok(result)
}

/// Read a category or label declaration — upstream `declaration`.
fn declaration(object: &Json, path: &str) -> Result<MarketDeclaration, MarketIndexError> {
    let title = object.get("title").ok_or_else(|| MarketIndexError {
        path: format!("{path}.title"),
        reason: "must be a string with a non-whitespace character".to_owned(),
    })?;
    Ok(MarketDeclaration {
        title: localized_text(title, &format!("{path}.title"))?,
    })
}

/// Read a sort-key declaration — upstream `sortKeyDeclaration`.
fn sort_key_declaration(
    object: &Json,
    path: &str,
) -> Result<MarketSortKeyDeclaration, MarketIndexError> {
    let order = match object.get("order") {
        Some(serde_json::Value::String(order)) if order == "asc" => SortDirection::Asc,
        Some(serde_json::Value::String(order)) if order == "desc" => SortDirection::Desc,
        _ => return fail(&format!("{path}.order"), "must be \"asc\" or \"desc\""),
    };
    let title = object.get("title").ok_or_else(|| MarketIndexError {
        path: format!("{path}.title"),
        reason: "must be a string with a non-whitespace character".to_owned(),
    })?;
    Ok(MarketSortKeyDeclaration {
        title: localized_text(title, &format!("{path}.title"))?,
        order,
    })
}

/// Read the index's declarations — upstream `indexMetadata`.
fn index_metadata(
    value: Option<&serde_json::Value>,
    path: &str,
) -> Result<MarketIndexMetadata, MarketIndexError> {
    let empty = Json::new();
    let object = match value.filter(|value| !value.is_null()) {
        Some(value) => object_at(value, path)?,
        None => &empty,
    };
    Ok(MarketIndexMetadata {
        categories: optional(object, "categories", path, |value, at| {
            declarations(value, at, declaration)
        })?
        .unwrap_or_default(),
        labels: optional(object, "labels", path, |value, at| {
            declarations(value, at, declaration)
        })?
        .unwrap_or_default(),
        sort_keys: optional(object, "sortKeys", path, |value, at| {
            declarations(value, at, sort_key_declaration)
        })?
        .unwrap_or_default(),
    })
}

/// Require a string that names a declared id — upstream `declaredId`.
fn declared_id<'v>(
    value: &'v serde_json::Value,
    path: &str,
    declared: &BTreeMap<String, impl Sized>,
) -> Result<&'v str, MarketIndexError> {
    let id = string_at(value, path)?;
    if !declared.contains_key(id) {
        return fail(path, format!("references undeclared id {}", quoted(id)));
    }
    Ok(id)
}

/// Render a string as JSON does, for a message that names it.
fn quoted(text: &str) -> String {
    serde_json::Value::String(text.to_owned()).to_string()
}

/// Read one plugin's market values — upstream `pluginMetadata`.
fn plugin_metadata(
    value: Option<&serde_json::Value>,
    path: &str,
    index: &MarketIndexMetadata,
) -> Result<MarketPluginMetadata, MarketIndexError> {
    let empty;
    let object = match value {
        Some(serde_json::Value::Null) | None => {
            empty = Json::new();
            &empty
        }
        Some(value) => object_at(value, path)?,
    };
    let category = match member(object, "category") {
        Some(value) => {
            Some(declared_id(value, &format!("{path}.category"), &index.categories)?.to_owned())
        }
        None => None,
    };
    let keywords = match member(object, "keywords") {
        Some(value) => {
            let at = format!("{path}.keywords");
            let array = as_array(value, &at)?;
            let mut keywords = Vec::with_capacity(array.len());
            for (position, keyword) in array.iter().enumerate() {
                match keyword {
                    serde_json::Value::String(keyword) => keywords.push(keyword.clone()),
                    _ => return fail(&format!("{at}[{position}]"), "must be a string"),
                }
            }
            keywords
        }
        None => Vec::new(),
    };
    let labels = match member(object, "labels") {
        Some(value) => {
            let at = format!("{path}.labels");
            let array = as_array(value, &at)?;
            let mut labels: Vec<String> = Vec::with_capacity(array.len());
            for (position, label) in array.iter().enumerate() {
                let id = declared_id(label, &format!("{at}[{position}]"), &index.labels)?;
                labels.push(id.to_owned());
            }
            let distinct: BTreeSet<&String> = labels.iter().collect();
            if distinct.len() != labels.len() {
                return fail(&at, "must not repeat a label");
            }
            labels
        }
        None => Vec::new(),
    };
    let sort_keys = match member(object, "sortKeys") {
        Some(value) => {
            let at = format!("{path}.sortKeys");
            let values = object_at(value, &at)?;
            let mut sort_keys = BTreeMap::new();
            for (id, sort_value) in values {
                declared_id(
                    &serde_json::Value::String(id.clone()),
                    &format!("{at}.{id}"),
                    &index.sort_keys,
                )?;
                if sort_value.is_null() {
                    continue;
                }
                let Some(number) = sort_value.as_f64().filter(|number| number.is_finite()) else {
                    return fail(&format!("{at}.{id}"), "must be a finite number");
                };
                sort_keys.insert(id.clone(), number);
            }
            sort_keys
        }
        None => BTreeMap::new(),
    };
    Ok(MarketPluginMetadata {
        category,
        keywords,
        labels,
        sort_keys,
    })
}

/// Require an array — upstream `if (!Array.isArray(member)) fail(at, 'must be an array')`.
fn as_array<'v>(
    value: &'v serde_json::Value,
    path: &str,
) -> Result<&'v Vec<serde_json::Value>, MarketIndexError> {
    match value {
        serde_json::Value::Array(items) => Ok(items),
        _ => fail(path, "must be an array"),
    }
}

/// Require an absolute URL with one of `schemes`, no credentials, and no query or fragment —
/// upstream `httpUrl`.
fn http_url(
    value: &serde_json::Value,
    path: &str,
    schemes: &[&str],
) -> Result<Url, MarketIndexError> {
    let text = string_at(value, path)?;
    let Some(url) = Url::parse(text) else {
        return fail(path, "must be an absolute URL");
    };
    if !schemes.contains(&url.scheme.as_str()) {
        return fail(path, format!("must use {}", schemes_with_colons(schemes)));
    }
    if !url.username.is_empty() || !url.password.is_empty() {
        return fail(path, "must not contain credentials");
    }
    if url.query.is_some() || url.fragment.is_some() {
        return fail(path, "must not contain a query or fragment");
    }
    Ok(url)
}

/// Render a scheme list with the colons upstream names them by.
fn schemes_with_colons(schemes: &[&str]) -> String {
    schemes
        .iter()
        .map(|scheme| format!("{scheme}:"))
        .collect::<Vec<String>>()
        .join(" or ")
}

/// Require an absolute http(s) URL, keeping its query and fragment — upstream `homepageUrl`, whose
/// comment notes that npm `homepage` values often carry a `#readme` fragment.
fn homepage_url(value: &serde_json::Value, path: &str) -> Result<String, MarketIndexError> {
    let text = string_at(value, path)?;
    let Some(url) = Url::parse(text) else {
        return fail(path, "must be an absolute URL");
    };
    if url.scheme != "https" && url.scheme != "http" {
        return fail(path, "must use https: or http:");
    }
    Ok(url.href())
}

/// Require a canonical https Git repository URL — upstream `gitUrl`.
///
/// The canonical-form check is what stops `https://GitHub.com/o/r`: the same repository spelled two
/// ways would be two different pinned origins, and a Git copy could never be updated across them.
fn git_url(value: &serde_json::Value, path: &str) -> Result<String, MarketIndexError> {
    let text = string_at(value, path)?;
    let url = http_url(value, path, &["https"])?;
    let href = url.href();
    if href != text {
        return fail(path, "must be a canonical URL");
    }
    // The rule is on the path as written: `^\/(?:[^/]+\/)*[^/]+$` needs at least two segments, so
    // `/o/r/` names one — a trailing slash ends an empty final segment, which is not a name.
    let segments: Vec<&str> = url
        .path
        .split('/')
        .filter(|part| !part.is_empty())
        .collect();
    if segments.len() < 2 || url.path.ends_with('/') {
        return fail(
            path,
            "must name a repository with at least two path segments",
        );
    }
    // The path is lowered first, so the comparison is already case-insensitive; the lint cannot
    // see that through the binding.
    #[allow(
        clippy::case_sensitive_file_extension_comparisons,
        reason = "the path this compares is the lowercased copy, so the comparison is case-insensitive by construction"
    )]
    let tarball = {
        let lowered = url.path.to_ascii_lowercase();
        lowered.ends_with(".tgz") || lowered.ends_with(".tar.gz")
    };
    if tarball {
        return fail(path, "must name a repository, not a tarball");
    }
    Ok(href)
}

/// Read a relative package directory — upstream `packagePath`.
fn package_path(value: &serde_json::Value, path: &str) -> Result<String, MarketIndexError> {
    let text = string_at(value, path)?;
    if !is_package_path(text) {
        return fail(
            path,
            "must be a relative directory of letters, digits, and ._@+- segments without . or ..",
        );
    }
    Ok(text.to_owned())
}

/// Read a plugin's source repository — upstream `repository`.
fn repository(value: &serde_json::Value, path: &str) -> Result<MarketRepository, MarketIndexError> {
    let object = object_at(value, path)?;
    let url = match object.get("url") {
        Some(url) => http_url(url, &format!("{path}.url"), &["https", "http"])?.href(),
        None => {
            return fail(
                &format!("{path}.url"),
                "must be a string with a non-whitespace character",
            );
        }
    };
    let directory = optional(object, "directory", path, package_path)?;
    Ok(MarketRepository { url, directory })
}

/// Read where one version installs from — upstream `source`.
fn source(value: &serde_json::Value, path: &str) -> Result<MarketSource, MarketIndexError> {
    let object = object_at(value, path)?;
    match object.get("type").and_then(serde_json::Value::as_str) {
        Some("npm") => {
            let registry = optional(object, "registry", path, |member, at| {
                http_url(member, at, &["https", "http"]).map(|url| url.href())
            })?;
            Ok(MarketSource::Npm { registry })
        }
        Some("git") => {
            let at = format!("{path}.commit");
            let commit = match object.get("commit") {
                Some(value) => string_at(value, &at)?,
                None => return fail(&at, "must be a string with a non-whitespace character"),
            };
            if !is_commit(commit) {
                return fail(&at, "must be a lowercase 40-hex-digit commit SHA");
            }
            let url = match object.get("url") {
                Some(value) => git_url(value, &format!("{path}.url"))?,
                None => {
                    return fail(
                        &format!("{path}.url"),
                        "must be a string with a non-whitespace character",
                    );
                }
            };
            let package_dir = optional(object, "path", path, package_path)?;
            Ok(MarketSource::Git {
                url,
                commit: commit.to_owned(),
                path: package_dir,
            })
        }
        _ => fail(&format!("{path}.type"), "must be \"npm\" or \"git\""),
    }
}

/// Read one version's runtime requirements — upstream `engines`.
fn engines(value: &serde_json::Value, path: &str) -> Result<MarketEngines, MarketIndexError> {
    let object = object_at(value, path)?;
    let dsh = optional(object, "dsh", path, |member, at| {
        let range = string_at(member, at)?;
        if range.starts_with("workspace:") {
            return fail(at, "must not be a workspace: range");
        }
        if !is_semver_range(range) {
            return fail(at, "must be a SemVer range");
        }
        Ok(range.to_owned())
    })?;
    Ok(MarketEngines { dsh })
}

/// Read one plugin's icon — upstream `icon`.
///
/// An inline `data:` icon must decode to at most [`MAX_INLINE_ICON_BYTES`], and a URL reference must
/// resolve to https or to the index's own origin: anything else would either inflate an index past
/// its fetch limit or leak a request to a third party.
fn icon(
    value: &serde_json::Value,
    path: &str,
    index_url: &Url,
) -> Result<String, MarketIndexError> {
    let text = string_at(value, path)?;
    if let Some(rest) = text.strip_prefix("data:") {
        let Some((media, payload)) = rest.split_once(";base64,") else {
            return fail(path, "must be a base64 SVG, PNG, JPEG, or WebP data URL");
        };
        if !matches!(
            media,
            "image/svg+xml" | "image/png" | "image/jpeg" | "image/webp"
        ) || payload.is_empty()
            || !is_base64(payload)
        {
            return fail(path, "must be a base64 SVG, PNG, JPEG, or WebP data URL");
        }
        let padding = if payload.ends_with("==") {
            2
        } else {
            usize::from(payload.ends_with('='))
        };
        let decoded = payload.len() / 4 * 3 - padding;
        if decoded > MAX_INLINE_ICON_BYTES {
            return fail(
                path,
                format!(
                    "must not exceed {} KiB of decoded bytes",
                    MAX_INLINE_ICON_BYTES / 1024
                ),
            );
        }
        return Ok(text.to_owned());
    }
    let Some(resolved) = resolve_url(text, index_url) else {
        return fail(path, "must be a URL reference");
    };
    let same_origin = match (resolved.origin(), index_url.origin()) {
        (Some(resolved), Some(index)) => resolved == index,
        _ => false,
    };
    if resolved.scheme != "https" && !same_origin {
        return fail(path, "must resolve to https or the index origin");
    }
    Ok(resolved.href())
}

/// Read one version — upstream `marketVersion`.
fn market_version(
    value: &serde_json::Value,
    path: &str,
) -> Result<MarketVersion, MarketIndexError> {
    let object = object_at(value, path)?;
    let version = match object.get("version") {
        Some(value) => string_at(value, &format!("{path}.version"))?,
        None => {
            return fail(
                &format!("{path}.version"),
                "must be a string with a non-whitespace character",
            );
        }
    };
    if !is_exact_semver(version) {
        return fail(
            &format!("{path}.version"),
            "must be an exact SemVer version",
        );
    }
    let at = format!("{path}.publishTimestamp");
    let Some(publish_timestamp) = object
        .get("publishTimestamp")
        .and_then(serde_json::Value::as_f64)
        .filter(|value| value.is_finite() && value.fract() == 0.0)
        .filter(|value| (0.0..MAX_PUBLISH_TIMESTAMP).contains(value))
    else {
        return fail(
            &at,
            "must be an integer number of Unix seconds in [0, 10^11)",
        );
    };
    let source = match object.get("source") {
        Some(value) => source(value, &format!("{path}.source"))?,
        None => return fail(&format!("{path}.source"), "must be an object"),
    };
    let engines = optional(object, "engines", path, engines)?.unwrap_or_default();
    Ok(MarketVersion {
        version: version.to_owned(),
        #[allow(
            clippy::cast_possible_truncation,
            reason = "the value is filtered to a finite integer in [0, 10^11) before this cast, so it is exactly representable and fits i64"
        )]
        publish_timestamp: publish_timestamp as i64,
        source,
        engines,
    })
}

/// Read a plugin's versions — upstream `versions`, which refuses an empty list and a repeated
/// version, then orders the result by **descending** `SemVer` precedence.
fn versions(value: &serde_json::Value, path: &str) -> Result<Vec<MarketVersion>, MarketIndexError> {
    let array = match value {
        serde_json::Value::Array(items) if !items.is_empty() => items,
        _ => return fail(path, "must be a non-empty array"),
    };
    let mut seen = BTreeSet::new();
    let mut parsed = Vec::with_capacity(array.len());
    for (position, member) in array.iter().enumerate() {
        let at = format!("{path}[{position}]");
        let entry = market_version(member, &at)?;
        if !seen.insert(entry.version.clone()) {
            return fail(
                &format!("{at}.version"),
                format!("duplicates {}", entry.version),
            );
        }
        parsed.push(entry);
    }
    parsed.sort_by(|left, right| compare_semver(&right.version, &left.version));
    Ok(parsed)
}

/// Read one plugin — upstream `plugin`.
fn plugin(
    value: &serde_json::Value,
    path: &str,
    index: &MarketIndexMetadata,
    index_url: &Url,
) -> Result<MarketPlugin, MarketIndexError> {
    let object = object_at(value, path)?;
    let at = format!("{path}.name");
    let name = match object.get("name") {
        Some(value) => string_at(value, &at)?,
        None => return fail(&at, "must be a string with a non-whitespace character"),
    };
    if name.len() > PACKAGE_NAME_MAX_LENGTH || !is_package_name(name) {
        return fail(&at, "must be a valid npm package name");
    }
    let title = match member(object, "title") {
        Some(value) => Some(localized_text(value, &format!("{path}.title"))?),
        None => None,
    };
    let description = match member(object, "description") {
        Some(value) => Some(localized_text(value, &format!("{path}.description"))?),
        None => None,
    };
    let icon_url = match member(object, "icon") {
        Some(value) => Some(icon(value, &format!("{path}.icon"), index_url)?),
        None => None,
    };
    let author = optional(object, "author", path, |value, at| {
        string_at(value, at).map(str::to_owned)
    })?;
    let homepage = optional(object, "homepage", path, homepage_url)?;
    let repo = optional(object, "repository", path, repository)?;
    let metadata = plugin_metadata(object.get("metadata"), &format!("{path}.metadata"), index)?;
    let versions = match object.get("versions") {
        Some(value) => versions(value, &format!("{path}.versions"))?,
        None => return fail(&format!("{path}.versions"), "must be a non-empty array"),
    };
    Ok(MarketPlugin {
        name: name.to_owned(),
        title,
        description,
        icon: icon_url,
        author,
        homepage,
        repository: repo,
        metadata,
        versions,
    })
}

/// Validate a parsed format-1 index — upstream `parseMarketIndex`.
///
/// Members the format does not define are ignored and dropped. Null plugin metadata fields normalize
/// like absent fields; null sort values are omitted while zero is retained. Each plugin's versions
/// come back in descending `SemVer` precedence.
///
/// # Errors
///
/// [`MarketIndexError`] naming the first violated rule, with the JSON path of the offending value.
pub fn parse_market_index(
    value: &serde_json::Value,
    index_url: &str,
) -> Result<MarketIndex, MarketIndexError> {
    let Some(base) = Url::parse(index_url) else {
        return fail(
            "$",
            format!("index URL {} must be an absolute URL", quoted(index_url)),
        );
    };
    let object = object_at(value, "$")?;
    if object.get("format").and_then(serde_json::Value::as_u64) != Some(1) {
        return fail("$.format", "must be 1");
    }
    let metadata = index_metadata(object.get("metadata"), "$.metadata")?;
    let plugins_value = match object.get("plugins") {
        Some(value) => as_array(value, "$.plugins")?,
        None => return fail("$.plugins", "must be an array"),
    };
    let mut names = BTreeSet::new();
    let mut plugins = Vec::with_capacity(plugins_value.len());
    for (position, entry) in plugins_value.iter().enumerate() {
        let at = format!("$.plugins[{position}]");
        let parsed = plugin(entry, &at, &metadata, &base)?;
        if !names.insert(parsed.name.clone()) {
            return fail(&format!("{at}.name"), format!("duplicates {}", parsed.name));
        }
        plugins.push(parsed);
    }
    let title = match object.get("title") {
        Some(value) => localized_text(value, "$.title")?,
        None => {
            return fail(
                "$.title",
                "must be a string with a non-whitespace character",
            );
        }
    };
    let description = match object.get("description") {
        Some(value) if !value.is_null() => Some(localized_text(value, "$.description")?),
        _ => None,
    };
    Ok(MarketIndex {
        format: 1,
        title,
        description,
        metadata,
        plugins,
    })
}

// ============================================================================================
// `SemVer` ordering and range syntax
// ============================================================================================

/// One parsed exact version, for the ordering `versions` needs.
#[derive(Clone, Debug, PartialEq, Eq)]
struct SemVerParts {
    major: u64,
    minor: u64,
    patch: u64,
    prerelease: Vec<PrereleaseIdentifier>,
}

/// One prerelease identifier: numeric ones compare numerically and sort before alphanumeric ones.
#[derive(Clone, Debug, PartialEq, Eq)]
enum PrereleaseIdentifier {
    /// A run of ASCII digits without a leading zero.
    Numeric(u64),
    /// An alphanumeric or hyphenated identifier.
    Alpha(String),
}

impl PrereleaseIdentifier {
    /// Compare two identifiers by `SemVer` precedence.
    fn precedence(&self, other: &Self) -> Ordering {
        match (self, other) {
            (Self::Numeric(left), Self::Numeric(right)) => left.cmp(right),
            (Self::Numeric(_), Self::Alpha(_)) => Ordering::Less,
            (Self::Alpha(_), Self::Numeric(_)) => Ordering::Greater,
            (Self::Alpha(left), Self::Alpha(right)) => left.cmp(right),
        }
    }
}

/// Parse an exact version for ordering. [`is_exact_semver`] has already accepted the text, so a
/// failure here is a programming error rather than a refused input; `None` keeps it total anyway.
fn semver_parts(text: &str) -> Option<SemVerParts> {
    let core = text.split(['-', '+']).next()?;
    let mut fields = core.split('.');
    let major = fields.next()?.parse().ok()?;
    let minor = fields.next()?.parse().ok()?;
    let patch = fields.next()?.parse().ok()?;
    let prerelease = text
        .split_once('-')
        .map(|(_, rest)| rest.split('+').next().unwrap_or(rest))
        .map(|rest| {
            rest.split('.')
                .map(|identifier| {
                    if identifier.bytes().all(|byte| byte.is_ascii_digit()) {
                        identifier.parse().map_or_else(
                            |_| PrereleaseIdentifier::Alpha(identifier.to_owned()),
                            PrereleaseIdentifier::Numeric,
                        )
                    } else {
                        PrereleaseIdentifier::Alpha(identifier.to_owned())
                    }
                })
                .collect()
        })
        .unwrap_or_default();
    Some(SemVerParts {
        major,
        minor,
        patch,
        prerelease,
    })
}

/// Compare two exact versions by `SemVer` precedence — build metadata is ignored, exactly as
/// `semver.compare` ignores it.
fn compare_semver(left: &str, right: &str) -> Ordering {
    let (Some(left), Some(right)) = (semver_parts(left), semver_parts(right)) else {
        return Ordering::Equal;
    };
    left.major
        .cmp(&right.major)
        .then_with(|| left.minor.cmp(&right.minor))
        .then_with(|| left.patch.cmp(&right.patch))
        .then_with(
            || match (left.prerelease.is_empty(), right.prerelease.is_empty()) {
                (true, true) => Ordering::Equal,
                // A version without a prerelease outranks one with it.
                (true, false) => Ordering::Greater,
                (false, true) => Ordering::Less,
                (false, false) => {
                    for (left, right) in left.prerelease.iter().zip(&right.prerelease) {
                        let ordering = left.precedence(right);
                        if ordering != Ordering::Equal {
                            return ordering;
                        }
                    }
                    left.prerelease.len().cmp(&right.prerelease.len())
                }
            },
        )
}

/// Whether `text` is a `SemVer` range **syntax** `node-semver` accepts — the check `engines.dsh`
/// makes through `semver.validRange`.
///
/// This validates grammar, not satisfaction: it answers "would `semver.validRange` return a range?"
/// and never "does version X satisfy it?". That split matters because `boot_gap.rs` deliberately has
/// no range *evaluator*; a syntax check cannot report a compatible plugin as incompatible, and an
/// `engines.dsh` string that failed it would otherwise be refused at parse time and take the whole
/// index with it.
///
/// The grammar is `node-semver`'s: `||`-separated sets of space-separated comparators, where each
/// comparator is an operator (`<`, `<=`, `>`, `>=`, `=`), a bare version, an x-range (`1`, `1.2`,
/// `1.x`, `*`), a tilde or caret range, or a hyphen range (`1.2.3 - 2.3.4`).
#[must_use]
pub fn is_semver_range(text: &str) -> bool {
    let text = text.trim();
    // An empty string and a lone `*` are both `*` to `node-semver`, and so is an empty alternative.
    if text.is_empty() {
        return true;
    }
    text.split("||").all(|set| {
        let set = set.trim();
        if set.is_empty() {
            return true;
        }
        // `node-semver` allows a space between a comparator operator and its version, so `>= 1.2.3`
        // is one comparator spelled as two tokens.
        let mut tokens: Vec<String> = set.split_whitespace().map(str::to_owned).collect();
        let mut index = 0;
        while index + 1 < tokens.len() {
            let joined = matches!(
                tokens[index].as_str(),
                ">" | ">=" | "<" | "<=" | "=" | "~" | "~>" | "^"
            );
            if joined && tokens[index + 1] != "-" {
                let combined = format!("{}{}", tokens[index], tokens[index + 1]);
                tokens.splice(index..=index + 1, [combined]);
            }
            index += 1;
        }
        let tokens: Vec<&str> = tokens.iter().map(String::as_str).collect();
        let mut index = 0;
        while index < tokens.len() {
            let token = tokens[index];
            // A hyphen range is three tokens: `lo - hi`.
            if tokens.get(index + 1) == Some(&"-") {
                let Some(high) = tokens.get(index + 2) else {
                    return false;
                };
                if !is_partial_version(token) || !is_partial_version(high) {
                    return false;
                }
                index += 3;
                continue;
            }
            if !is_comparator(token) {
                return false;
            }
            index += 1;
        }
        true
    })
}

/// Whether one token is a comparator, an x-range, or a tilde/caret range.
fn is_comparator(token: &str) -> bool {
    let token = token.trim();
    if token.is_empty() || token == "*" || token == "x" || token == "X" {
        return true;
    }
    if let Some(rest) = token.strip_prefix("~>") {
        return is_partial_version(rest.trim());
    }
    if let Some(rest) = token
        .strip_prefix("~")
        .or_else(|| token.strip_prefix("^"))
        .or_else(|| token.strip_prefix(">="))
        .or_else(|| token.strip_prefix("<="))
        .or_else(|| token.strip_prefix(">"))
        .or_else(|| token.strip_prefix("<"))
        .or_else(|| token.strip_prefix("="))
    {
        return is_partial_version(rest.trim());
    }
    is_partial_version(token)
}

/// Whether `text` is a version or a partial/x-range version: 1 to 3 dot-separated fields, each a
/// number or an `x`/`X`/`*` wildcard, optionally followed by a prerelease and build metadata.
///
/// A wildcard may not appear before a concrete field (`1.x.3` is not a range `node-semver` accepts).
fn is_partial_version(text: &str) -> bool {
    let (core, tail) = split_version_tail(text);
    let mut wildcard_seen = false;
    let mut count = 0;
    for field in core.split('.') {
        count += 1;
        if count > 3 {
            return false;
        }
        if field.is_empty() {
            return false;
        }
        if matches!(field, "x" | "X" | "*") {
            wildcard_seen = true;
            continue;
        }
        if wildcard_seen {
            return false;
        }
        if !field.bytes().all(|byte| byte.is_ascii_digit()) {
            return false;
        }
        // A leading zero is not a version `node-semver` parses.
        if field.len() > 1 && field.starts_with('0') {
            return false;
        }
    }
    if count == 0 {
        return false;
    }
    match tail {
        VersionTail::None => true,
        VersionTail::Build(build) => {
            !build.is_empty() && is_identifier_run(build) && !wildcard_seen
        }
        VersionTail::Prerelease(prerelease, build) => {
            !wildcard_seen
                && !prerelease.is_empty()
                && is_identifier_run(prerelease)
                && build.is_none_or(|build| !build.is_empty() && is_identifier_run(build))
        }
    }
}

/// The optional prerelease and build metadata after a version core.
enum VersionTail<'a> {
    /// Neither.
    None,
    /// Only build metadata.
    Build(&'a str),
    /// A prerelease, with build metadata when it follows.
    Prerelease(&'a str, Option<&'a str>),
}

/// Split a version's core from its prerelease and build metadata.
fn split_version_tail(text: &str) -> (&str, VersionTail<'_>) {
    let (rest, build) = match text.split_once('+') {
        Some((rest, build)) => (rest, Some(build)),
        None => (text, None),
    };
    match rest.split_once('-') {
        Some((core, prerelease)) => (core, VersionTail::Prerelease(prerelease, build)),
        None => match build {
            Some(build) => (rest, VersionTail::Build(build)),
            None => (rest, VersionTail::None),
        },
    }
}

// ============================================================================================
// Relative URL resolution — the part of WHATWG URL that `icon` and `region` need
// ============================================================================================

/// Resolve a URL reference against a base — the subset of `new URL(reference, base)` that matters
/// for an icon reference: an absolute URL, a network-path reference (`//host/p`), an origin-relative
/// path (`/p`), or a path relative to the base's directory.
#[must_use]
pub fn resolve_url(reference: &str, base: &Url) -> Option<Url> {
    if let Some(url) = Url::parse(reference) {
        return Some(url);
    }
    let reference = reference.trim();
    if reference.starts_with("//") {
        return Url::parse(&format!("{}:{reference}", base.scheme));
    }
    // A reference that carries an authority of its own but does not parse (`https://[`) is not a
    // resolvable reference at all — `new URL` throws, which is the reference rule.
    if reference.contains("://") || reference.ends_with("://") {
        return None;
    }
    // Any other scheme prefix is a URL this subset refuses rather than a relative path.
    if let Some((scheme, _)) = reference.split_once(':')
        && scheme.starts_with(|ch: char| ch.is_ascii_alphabetic())
        && scheme
            .chars()
            .all(|ch| ch.is_ascii_alphanumeric() || matches!(ch, '+' | '-' | '.'))
    {
        return None;
    }
    let (before_fragment, fragment) = match reference.split_once('#') {
        Some((head, rest)) => (head, Some(rest)),
        None => (reference, None),
    };
    let (path_reference, query) = match before_fragment.split_once('?') {
        Some((head, rest)) => (head, Some(rest)),
        None => (before_fragment, None),
    };
    let path = if path_reference.starts_with('/') {
        normalize_path(path_reference)
    } else if path_reference.is_empty() {
        base.path.clone()
    } else {
        let directory = match base.path.rfind('/') {
            Some(position) => &base.path[..=position],
            None => "/",
        };
        normalize_path(&format!("{directory}{path_reference}"))
    };
    Some(Url {
        scheme: base.scheme.clone(),
        username: base.username.clone(),
        password: base.password.clone(),
        host: base.host.clone(),
        port: base.port,
        path,
        query: match query {
            Some(query) => Some(query.to_owned()),
            None => base.query.clone(),
        },
        fragment: fragment.map(str::to_owned),
    })
}

// ============================================================================================
// Index bodies — upstream `parse.ts`
// ============================================================================================

/// A validated index, or the JSON path, rule, and diagnostic of the first violation — upstream
/// `IndexParseResult`.
///
/// `$` as the path covers both a UTF-8 and a JSON failure: neither has a path inside the document,
/// because there is no document yet.
#[derive(Clone, Debug, PartialEq)]
pub enum IndexParseResult {
    /// The body validated.
    Ok(MarketIndex),
    /// The body did not validate; `message` is what a person sees.
    Invalid {
        /// JSON path of the first violation, or `$` when the body itself was unreadable.
        path: String,
        /// The violated rule.
        reason: String,
        /// The diagnostic, which for a format violation is `"{path} {reason}"`.
        message: String,
    },
}

/// Decode a body as strict UTF-8, parse it as JSON, and validate it as format 1 — upstream
/// `parseIndexBody`.
///
/// Strict UTF-8 is deliberate: a JSON document that is not valid UTF-8 has already been damaged by
/// whatever produced it, and decoding it lossily would hand the validator strings that no longer
/// match what the publisher wrote.
///
/// # Examples
///
/// ```
/// # use girder_core::plugin_market_gap::{IndexParseResult, parse_index_body};
/// let body = br#"{"format":1,"title":"M","plugins":[{"name":"p","icon":"p.svg",
///     "versions":[{"version":"1.0.0","publishTimestamp":1,"source":{"type":"npm"}}]}]}"#;
/// let IndexParseResult::Ok(index) = parse_index_body(body, "https://market.example/index.json") else {
///     panic!("the example index must validate");
/// };
/// assert_eq!(index.plugins[0].icon.as_deref(), Some("https://market.example/p.svg"));
/// ```
#[must_use]
pub fn parse_index_body(body: &[u8], url: &str) -> IndexParseResult {
    let Ok(text) = std::str::from_utf8(body) else {
        return IndexParseResult::Invalid {
            path: "$".to_owned(),
            reason: "must be UTF-8".to_owned(),
            message: "must be UTF-8".to_owned(),
        };
    };
    let value: serde_json::Value = match serde_json::from_str(text) {
        Ok(value) => value,
        Err(error) => {
            let reason = format!("must be JSON: {error}");
            return IndexParseResult::Invalid {
                path: "$".to_owned(),
                reason: reason.clone(),
                message: reason,
            };
        }
    };
    match parse_market_index(&value, url) {
        Ok(index) => IndexParseResult::Ok(index),
        Err(error) => IndexParseResult::Invalid {
            path: error.path.clone(),
            reason: error.reason.clone(),
            message: error.to_string(),
        },
    }
}
/// Code points whose `Script_Extensions` name Han, Hiragana, Katakana, or Hangul and whose general
/// category is a letter, mark, or digit — the runs [`search_terms`] splits into character pairs.
#[rustfmt::skip]
const CJK_SCRIPT_RANGES: &[(u32, u32)] = &[
    (0x0305, 0x0305), (0x0323, 0x0323), (0x1100, 0x11FF), (0x3005, 0x3007),
    (0x3021, 0x302F), (0x3031, 0x3035), (0x3038, 0x303C), (0x3041, 0x3096),
    (0x3099, 0x309A), (0x309D, 0x309F), (0x30A1, 0x30FA), (0x30FC, 0x30FF),
    (0x3131, 0x318E), (0x3192, 0x3195), (0x31F0, 0x31FF), (0x3220, 0x3229),
    (0x3280, 0x3289), (0x3400, 0x4DBF), (0x4E00, 0x9FFF), (0xA960, 0xA97C),
    (0xAC00, 0xD7A3), (0xD7B0, 0xD7C6), (0xD7CB, 0xD7FB), (0xF900, 0xFA6D),
    (0xFA70, 0xFAD9), (0xFF66, 0xFFBE), (0xFFC2, 0xFFC7), (0xFFCA, 0xFFCF),
    (0xFFD2, 0xFFD7), (0xFFDA, 0xFFDC), (0x16FE3, 0x16FE3), (0x16FF0, 0x16FF1),
    (0x1AFF0, 0x1AFF3), (0x1AFF5, 0x1AFFB), (0x1AFFD, 0x1AFFE), (0x1B000, 0x1B122),
    (0x1B132, 0x1B132), (0x1B150, 0x1B152), (0x1B155, 0x1B155), (0x1B164, 0x1B167),
    (0x1D360, 0x1D371), (0x20000, 0x2A6DF), (0x2A700, 0x2B739), (0x2B740, 0x2B81D),
    (0x2B820, 0x2CEA1), (0x2CEB0, 0x2EBE0), (0x2EBF0, 0x2EE5D), (0x2F800, 0x2FA1D),
    (0x30000, 0x3134A), (0x31350, 0x323AF),
];

/// Code points whose general category is a letter, mark, or digit — the characters a term is made of.
#[rustfmt::skip]
const WORD_RANGES: &[(u32, u32)] = &[
    (0x0030, 0x0039), (0x0041, 0x005A), (0x0061, 0x007A), (0x00AA, 0x00AA),
    (0x00B2, 0x00B3), (0x00B5, 0x00B5), (0x00B9, 0x00BA), (0x00BC, 0x00BE),
    (0x00C0, 0x00D6), (0x00D8, 0x00F6), (0x00F8, 0x02C1), (0x02C6, 0x02D1),
    (0x02E0, 0x02E4), (0x02EC, 0x02EC), (0x02EE, 0x02EE), (0x0300, 0x0374),
    (0x0376, 0x0377), (0x037A, 0x037D), (0x037F, 0x037F), (0x0386, 0x0386),
    (0x0388, 0x038A), (0x038C, 0x038C), (0x038E, 0x03A1), (0x03A3, 0x03F5),
    (0x03F7, 0x0481), (0x0483, 0x052F), (0x0531, 0x0556), (0x0559, 0x0559),
    (0x0560, 0x0588), (0x0591, 0x05BD), (0x05BF, 0x05BF), (0x05C1, 0x05C2),
    (0x05C4, 0x05C5), (0x05C7, 0x05C7), (0x05D0, 0x05EA), (0x05EF, 0x05F2),
    (0x0610, 0x061A), (0x0620, 0x0669), (0x066E, 0x06D3), (0x06D5, 0x06DC),
    (0x06DF, 0x06E8), (0x06EA, 0x06FC), (0x06FF, 0x06FF), (0x0710, 0x074A),
    (0x074D, 0x07B1), (0x07C0, 0x07F5), (0x07FA, 0x07FA), (0x07FD, 0x07FD),
    (0x0800, 0x082D), (0x0840, 0x085B), (0x0860, 0x086A), (0x0870, 0x0887),
    (0x0889, 0x088E), (0x0897, 0x08E1), (0x08E3, 0x0963), (0x0966, 0x096F),
    (0x0971, 0x0983), (0x0985, 0x098C), (0x098F, 0x0990), (0x0993, 0x09A8),
    (0x09AA, 0x09B0), (0x09B2, 0x09B2), (0x09B6, 0x09B9), (0x09BC, 0x09C4),
    (0x09C7, 0x09C8), (0x09CB, 0x09CE), (0x09D7, 0x09D7), (0x09DC, 0x09DD),
    (0x09DF, 0x09E3), (0x09E6, 0x09F1), (0x09F4, 0x09F9), (0x09FC, 0x09FC),
    (0x09FE, 0x09FE), (0x0A01, 0x0A03), (0x0A05, 0x0A0A), (0x0A0F, 0x0A10),
    (0x0A13, 0x0A28), (0x0A2A, 0x0A30), (0x0A32, 0x0A33), (0x0A35, 0x0A36),
    (0x0A38, 0x0A39), (0x0A3C, 0x0A3C), (0x0A3E, 0x0A42), (0x0A47, 0x0A48),
    (0x0A4B, 0x0A4D), (0x0A51, 0x0A51), (0x0A59, 0x0A5C), (0x0A5E, 0x0A5E),
    (0x0A66, 0x0A75), (0x0A81, 0x0A83), (0x0A85, 0x0A8D), (0x0A8F, 0x0A91),
    (0x0A93, 0x0AA8), (0x0AAA, 0x0AB0), (0x0AB2, 0x0AB3), (0x0AB5, 0x0AB9),
    (0x0ABC, 0x0AC5), (0x0AC7, 0x0AC9), (0x0ACB, 0x0ACD), (0x0AD0, 0x0AD0),
    (0x0AE0, 0x0AE3), (0x0AE6, 0x0AEF), (0x0AF9, 0x0AFF), (0x0B01, 0x0B03),
    (0x0B05, 0x0B0C), (0x0B0F, 0x0B10), (0x0B13, 0x0B28), (0x0B2A, 0x0B30),
    (0x0B32, 0x0B33), (0x0B35, 0x0B39), (0x0B3C, 0x0B44), (0x0B47, 0x0B48),
    (0x0B4B, 0x0B4D), (0x0B55, 0x0B57), (0x0B5C, 0x0B5D), (0x0B5F, 0x0B63),
    (0x0B66, 0x0B6F), (0x0B71, 0x0B77), (0x0B82, 0x0B83), (0x0B85, 0x0B8A),
    (0x0B8E, 0x0B90), (0x0B92, 0x0B95), (0x0B99, 0x0B9A), (0x0B9C, 0x0B9C),
    (0x0B9E, 0x0B9F), (0x0BA3, 0x0BA4), (0x0BA8, 0x0BAA), (0x0BAE, 0x0BB9),
    (0x0BBE, 0x0BC2), (0x0BC6, 0x0BC8), (0x0BCA, 0x0BCD), (0x0BD0, 0x0BD0),
    (0x0BD7, 0x0BD7), (0x0BE6, 0x0BF2), (0x0C00, 0x0C0C), (0x0C0E, 0x0C10),
    (0x0C12, 0x0C28), (0x0C2A, 0x0C39), (0x0C3C, 0x0C44), (0x0C46, 0x0C48),
    (0x0C4A, 0x0C4D), (0x0C55, 0x0C56), (0x0C58, 0x0C5A), (0x0C5D, 0x0C5D),
    (0x0C60, 0x0C63), (0x0C66, 0x0C6F), (0x0C78, 0x0C7E), (0x0C80, 0x0C83),
    (0x0C85, 0x0C8C), (0x0C8E, 0x0C90), (0x0C92, 0x0CA8), (0x0CAA, 0x0CB3),
    (0x0CB5, 0x0CB9), (0x0CBC, 0x0CC4), (0x0CC6, 0x0CC8), (0x0CCA, 0x0CCD),
    (0x0CD5, 0x0CD6), (0x0CDD, 0x0CDE), (0x0CE0, 0x0CE3), (0x0CE6, 0x0CEF),
    (0x0CF1, 0x0CF3), (0x0D00, 0x0D0C), (0x0D0E, 0x0D10), (0x0D12, 0x0D44),
    (0x0D46, 0x0D48), (0x0D4A, 0x0D4E), (0x0D54, 0x0D63), (0x0D66, 0x0D78),
    (0x0D7A, 0x0D7F), (0x0D81, 0x0D83), (0x0D85, 0x0D96), (0x0D9A, 0x0DB1),
    (0x0DB3, 0x0DBB), (0x0DBD, 0x0DBD), (0x0DC0, 0x0DC6), (0x0DCA, 0x0DCA),
    (0x0DCF, 0x0DD4), (0x0DD6, 0x0DD6), (0x0DD8, 0x0DDF), (0x0DE6, 0x0DEF),
    (0x0DF2, 0x0DF3), (0x0E01, 0x0E3A), (0x0E40, 0x0E4E), (0x0E50, 0x0E59),
    (0x0E81, 0x0E82), (0x0E84, 0x0E84), (0x0E86, 0x0E8A), (0x0E8C, 0x0EA3),
    (0x0EA5, 0x0EA5), (0x0EA7, 0x0EBD), (0x0EC0, 0x0EC4), (0x0EC6, 0x0EC6),
    (0x0EC8, 0x0ECE), (0x0ED0, 0x0ED9), (0x0EDC, 0x0EDF), (0x0F00, 0x0F00),
    (0x0F18, 0x0F19), (0x0F20, 0x0F33), (0x0F35, 0x0F35), (0x0F37, 0x0F37),
    (0x0F39, 0x0F39), (0x0F3E, 0x0F47), (0x0F49, 0x0F6C), (0x0F71, 0x0F84),
    (0x0F86, 0x0F97), (0x0F99, 0x0FBC), (0x0FC6, 0x0FC6), (0x1000, 0x1049),
    (0x1050, 0x109D), (0x10A0, 0x10C5), (0x10C7, 0x10C7), (0x10CD, 0x10CD),
    (0x10D0, 0x10FA), (0x10FC, 0x1248), (0x124A, 0x124D), (0x1250, 0x1256),
    (0x1258, 0x1258), (0x125A, 0x125D), (0x1260, 0x1288), (0x128A, 0x128D),
    (0x1290, 0x12B0), (0x12B2, 0x12B5), (0x12B8, 0x12BE), (0x12C0, 0x12C0),
    (0x12C2, 0x12C5), (0x12C8, 0x12D6), (0x12D8, 0x1310), (0x1312, 0x1315),
    (0x1318, 0x135A), (0x135D, 0x135F), (0x1369, 0x137C), (0x1380, 0x138F),
    (0x13A0, 0x13F5), (0x13F8, 0x13FD), (0x1401, 0x166C), (0x166F, 0x167F),
    (0x1681, 0x169A), (0x16A0, 0x16EA), (0x16EE, 0x16F8), (0x1700, 0x1715),
    (0x171F, 0x1734), (0x1740, 0x1753), (0x1760, 0x176C), (0x176E, 0x1770),
    (0x1772, 0x1773), (0x1780, 0x17D3), (0x17D7, 0x17D7), (0x17DC, 0x17DD),
    (0x17E0, 0x17E9), (0x17F0, 0x17F9), (0x180B, 0x180D), (0x180F, 0x1819),
    (0x1820, 0x1878), (0x1880, 0x18AA), (0x18B0, 0x18F5), (0x1900, 0x191E),
    (0x1920, 0x192B), (0x1930, 0x193B), (0x1946, 0x196D), (0x1970, 0x1974),
    (0x1980, 0x19AB), (0x19B0, 0x19C9), (0x19D0, 0x19DA), (0x1A00, 0x1A1B),
    (0x1A20, 0x1A5E), (0x1A60, 0x1A7C), (0x1A7F, 0x1A89), (0x1A90, 0x1A99),
    (0x1AA7, 0x1AA7), (0x1AB0, 0x1ACE), (0x1B00, 0x1B4C), (0x1B50, 0x1B59),
    (0x1B6B, 0x1B73), (0x1B80, 0x1BF3), (0x1C00, 0x1C37), (0x1C40, 0x1C49),
    (0x1C4D, 0x1C7D), (0x1C80, 0x1C8A), (0x1C90, 0x1CBA), (0x1CBD, 0x1CBF),
    (0x1CD0, 0x1CD2), (0x1CD4, 0x1CFA), (0x1D00, 0x1F15), (0x1F18, 0x1F1D),
    (0x1F20, 0x1F45), (0x1F48, 0x1F4D), (0x1F50, 0x1F57), (0x1F59, 0x1F59),
    (0x1F5B, 0x1F5B), (0x1F5D, 0x1F5D), (0x1F5F, 0x1F7D), (0x1F80, 0x1FB4),
    (0x1FB6, 0x1FBC), (0x1FBE, 0x1FBE), (0x1FC2, 0x1FC4), (0x1FC6, 0x1FCC),
    (0x1FD0, 0x1FD3), (0x1FD6, 0x1FDB), (0x1FE0, 0x1FEC), (0x1FF2, 0x1FF4),
    (0x1FF6, 0x1FFC), (0x2070, 0x2071), (0x2074, 0x2079), (0x207F, 0x2089),
    (0x2090, 0x209C), (0x20D0, 0x20F0), (0x2102, 0x2102), (0x2107, 0x2107),
    (0x210A, 0x2113), (0x2115, 0x2115), (0x2119, 0x211D), (0x2124, 0x2124),
    (0x2126, 0x2126), (0x2128, 0x2128), (0x212A, 0x212D), (0x212F, 0x2139),
    (0x213C, 0x213F), (0x2145, 0x2149), (0x214E, 0x214E), (0x2150, 0x2189),
    (0x2460, 0x249B), (0x24EA, 0x24FF), (0x2776, 0x2793), (0x2C00, 0x2CE4),
    (0x2CEB, 0x2CF3), (0x2CFD, 0x2CFD), (0x2D00, 0x2D25), (0x2D27, 0x2D27),
    (0x2D2D, 0x2D2D), (0x2D30, 0x2D67), (0x2D6F, 0x2D6F), (0x2D7F, 0x2D96),
    (0x2DA0, 0x2DA6), (0x2DA8, 0x2DAE), (0x2DB0, 0x2DB6), (0x2DB8, 0x2DBE),
    (0x2DC0, 0x2DC6), (0x2DC8, 0x2DCE), (0x2DD0, 0x2DD6), (0x2DD8, 0x2DDE),
    (0x2DE0, 0x2DFF), (0x2E2F, 0x2E2F), (0x3005, 0x3007), (0x3021, 0x302F),
    (0x3031, 0x3035), (0x3038, 0x303C), (0x3041, 0x3096), (0x3099, 0x309A),
    (0x309D, 0x309F), (0x30A1, 0x30FA), (0x30FC, 0x30FF), (0x3105, 0x312F),
    (0x3131, 0x318E), (0x3192, 0x3195), (0x31A0, 0x31BF), (0x31F0, 0x31FF),
    (0x3220, 0x3229), (0x3248, 0x324F), (0x3251, 0x325F), (0x3280, 0x3289),
    (0x32B1, 0x32BF), (0x3400, 0x4DBF), (0x4E00, 0xA48C), (0xA4D0, 0xA4FD),
    (0xA500, 0xA60C), (0xA610, 0xA62B), (0xA640, 0xA672), (0xA674, 0xA67D),
    (0xA67F, 0xA6F1), (0xA717, 0xA71F), (0xA722, 0xA788), (0xA78B, 0xA7CD),
    (0xA7D0, 0xA7D1), (0xA7D3, 0xA7D3), (0xA7D5, 0xA7DC), (0xA7F2, 0xA827),
    (0xA82C, 0xA82C), (0xA830, 0xA835), (0xA840, 0xA873), (0xA880, 0xA8C5),
    (0xA8D0, 0xA8D9), (0xA8E0, 0xA8F7), (0xA8FB, 0xA8FB), (0xA8FD, 0xA92D),
    (0xA930, 0xA953), (0xA960, 0xA97C), (0xA980, 0xA9C0), (0xA9CF, 0xA9D9),
    (0xA9E0, 0xA9FE), (0xAA00, 0xAA36), (0xAA40, 0xAA4D), (0xAA50, 0xAA59),
    (0xAA60, 0xAA76), (0xAA7A, 0xAAC2), (0xAADB, 0xAADD), (0xAAE0, 0xAAEF),
    (0xAAF2, 0xAAF6), (0xAB01, 0xAB06), (0xAB09, 0xAB0E), (0xAB11, 0xAB16),
    (0xAB20, 0xAB26), (0xAB28, 0xAB2E), (0xAB30, 0xAB5A), (0xAB5C, 0xAB69),
    (0xAB70, 0xABEA), (0xABEC, 0xABED), (0xABF0, 0xABF9), (0xAC00, 0xD7A3),
    (0xD7B0, 0xD7C6), (0xD7CB, 0xD7FB), (0xF900, 0xFA6D), (0xFA70, 0xFAD9),
    (0xFB00, 0xFB06), (0xFB13, 0xFB17), (0xFB1D, 0xFB28), (0xFB2A, 0xFB36),
    (0xFB38, 0xFB3C), (0xFB3E, 0xFB3E), (0xFB40, 0xFB41), (0xFB43, 0xFB44),
    (0xFB46, 0xFBB1), (0xFBD3, 0xFD3D), (0xFD50, 0xFD8F), (0xFD92, 0xFDC7),
    (0xFDF0, 0xFDFB), (0xFE00, 0xFE0F), (0xFE20, 0xFE2F), (0xFE70, 0xFE74),
    (0xFE76, 0xFEFC), (0xFF10, 0xFF19), (0xFF21, 0xFF3A), (0xFF41, 0xFF5A),
    (0xFF66, 0xFFBE), (0xFFC2, 0xFFC7), (0xFFCA, 0xFFCF), (0xFFD2, 0xFFD7),
    (0xFFDA, 0xFFDC), (0x10000, 0x1000B), (0x1000D, 0x10026), (0x10028, 0x1003A),
    (0x1003C, 0x1003D), (0x1003F, 0x1004D), (0x10050, 0x1005D), (0x10080, 0x100FA),
    (0x10107, 0x10133), (0x10140, 0x10178), (0x1018A, 0x1018B), (0x101FD, 0x101FD),
    (0x10280, 0x1029C), (0x102A0, 0x102D0), (0x102E0, 0x102FB), (0x10300, 0x10323),
    (0x1032D, 0x1034A), (0x10350, 0x1037A), (0x10380, 0x1039D), (0x103A0, 0x103C3),
    (0x103C8, 0x103CF), (0x103D1, 0x103D5), (0x10400, 0x1049D), (0x104A0, 0x104A9),
    (0x104B0, 0x104D3), (0x104D8, 0x104FB), (0x10500, 0x10527), (0x10530, 0x10563),
    (0x10570, 0x1057A), (0x1057C, 0x1058A), (0x1058C, 0x10592), (0x10594, 0x10595),
    (0x10597, 0x105A1), (0x105A3, 0x105B1), (0x105B3, 0x105B9), (0x105BB, 0x105BC),
    (0x105C0, 0x105F3), (0x10600, 0x10736), (0x10740, 0x10755), (0x10760, 0x10767),
    (0x10780, 0x10785), (0x10787, 0x107B0), (0x107B2, 0x107BA), (0x10800, 0x10805),
    (0x10808, 0x10808), (0x1080A, 0x10835), (0x10837, 0x10838), (0x1083C, 0x1083C),
    (0x1083F, 0x10855), (0x10858, 0x10876), (0x10879, 0x1089E), (0x108A7, 0x108AF),
    (0x108E0, 0x108F2), (0x108F4, 0x108F5), (0x108FB, 0x1091B), (0x10920, 0x10939),
    (0x10980, 0x109B7), (0x109BC, 0x109CF), (0x109D2, 0x10A03), (0x10A05, 0x10A06),
    (0x10A0C, 0x10A13), (0x10A15, 0x10A17), (0x10A19, 0x10A35), (0x10A38, 0x10A3A),
    (0x10A3F, 0x10A48), (0x10A60, 0x10A7E), (0x10A80, 0x10A9F), (0x10AC0, 0x10AC7),
    (0x10AC9, 0x10AE6), (0x10AEB, 0x10AEF), (0x10B00, 0x10B35), (0x10B40, 0x10B55),
    (0x10B58, 0x10B72), (0x10B78, 0x10B91), (0x10BA9, 0x10BAF), (0x10C00, 0x10C48),
    (0x10C80, 0x10CB2), (0x10CC0, 0x10CF2), (0x10CFA, 0x10D27), (0x10D30, 0x10D39),
    (0x10D40, 0x10D65), (0x10D69, 0x10D6D), (0x10D6F, 0x10D85), (0x10E60, 0x10E7E),
    (0x10E80, 0x10EA9), (0x10EAB, 0x10EAC), (0x10EB0, 0x10EB1), (0x10EC2, 0x10EC4),
    (0x10EFC, 0x10F27), (0x10F30, 0x10F54), (0x10F70, 0x10F85), (0x10FB0, 0x10FCB),
    (0x10FE0, 0x10FF6), (0x11000, 0x11046), (0x11052, 0x11075), (0x1107F, 0x110BA),
    (0x110C2, 0x110C2), (0x110D0, 0x110E8), (0x110F0, 0x110F9), (0x11100, 0x11134),
    (0x11136, 0x1113F), (0x11144, 0x11147), (0x11150, 0x11173), (0x11176, 0x11176),
    (0x11180, 0x111C4), (0x111C9, 0x111CC), (0x111CE, 0x111DA), (0x111DC, 0x111DC),
    (0x111E1, 0x111F4), (0x11200, 0x11211), (0x11213, 0x11237), (0x1123E, 0x11241),
    (0x11280, 0x11286), (0x11288, 0x11288), (0x1128A, 0x1128D), (0x1128F, 0x1129D),
    (0x1129F, 0x112A8), (0x112B0, 0x112EA), (0x112F0, 0x112F9), (0x11300, 0x11303),
    (0x11305, 0x1130C), (0x1130F, 0x11310), (0x11313, 0x11328), (0x1132A, 0x11330),
    (0x11332, 0x11333), (0x11335, 0x11339), (0x1133B, 0x11344), (0x11347, 0x11348),
    (0x1134B, 0x1134D), (0x11350, 0x11350), (0x11357, 0x11357), (0x1135D, 0x11363),
    (0x11366, 0x1136C), (0x11370, 0x11374), (0x11380, 0x11389), (0x1138B, 0x1138B),
    (0x1138E, 0x1138E), (0x11390, 0x113B5), (0x113B7, 0x113C0), (0x113C2, 0x113C2),
    (0x113C5, 0x113C5), (0x113C7, 0x113CA), (0x113CC, 0x113D3), (0x113E1, 0x113E2),
    (0x11400, 0x1144A), (0x11450, 0x11459), (0x1145E, 0x11461), (0x11480, 0x114C5),
    (0x114C7, 0x114C7), (0x114D0, 0x114D9), (0x11580, 0x115B5), (0x115B8, 0x115C0),
    (0x115D8, 0x115DD), (0x11600, 0x11640), (0x11644, 0x11644), (0x11650, 0x11659),
    (0x11680, 0x116B8), (0x116C0, 0x116C9), (0x116D0, 0x116E3), (0x11700, 0x1171A),
    (0x1171D, 0x1172B), (0x11730, 0x1173B), (0x11740, 0x11746), (0x11800, 0x1183A),
    (0x118A0, 0x118F2), (0x118FF, 0x11906), (0x11909, 0x11909), (0x1190C, 0x11913),
    (0x11915, 0x11916), (0x11918, 0x11935), (0x11937, 0x11938), (0x1193B, 0x11943),
    (0x11950, 0x11959), (0x119A0, 0x119A7), (0x119AA, 0x119D7), (0x119DA, 0x119E1),
    (0x119E3, 0x119E4), (0x11A00, 0x11A3E), (0x11A47, 0x11A47), (0x11A50, 0x11A99),
    (0x11A9D, 0x11A9D), (0x11AB0, 0x11AF8), (0x11BC0, 0x11BE0), (0x11BF0, 0x11BF9),
    (0x11C00, 0x11C08), (0x11C0A, 0x11C36), (0x11C38, 0x11C40), (0x11C50, 0x11C6C),
    (0x11C72, 0x11C8F), (0x11C92, 0x11CA7), (0x11CA9, 0x11CB6), (0x11D00, 0x11D06),
    (0x11D08, 0x11D09), (0x11D0B, 0x11D36), (0x11D3A, 0x11D3A), (0x11D3C, 0x11D3D),
    (0x11D3F, 0x11D47), (0x11D50, 0x11D59), (0x11D60, 0x11D65), (0x11D67, 0x11D68),
    (0x11D6A, 0x11D8E), (0x11D90, 0x11D91), (0x11D93, 0x11D98), (0x11DA0, 0x11DA9),
    (0x11EE0, 0x11EF6), (0x11F00, 0x11F10), (0x11F12, 0x11F3A), (0x11F3E, 0x11F42),
    (0x11F50, 0x11F5A), (0x11FB0, 0x11FB0), (0x11FC0, 0x11FD4), (0x12000, 0x12399),
    (0x12400, 0x1246E), (0x12480, 0x12543), (0x12F90, 0x12FF0), (0x13000, 0x1342F),
    (0x13440, 0x13455), (0x13460, 0x143FA), (0x14400, 0x14646), (0x16100, 0x16139),
    (0x16800, 0x16A38), (0x16A40, 0x16A5E), (0x16A60, 0x16A69), (0x16A70, 0x16ABE),
    (0x16AC0, 0x16AC9), (0x16AD0, 0x16AED), (0x16AF0, 0x16AF4), (0x16B00, 0x16B36),
    (0x16B40, 0x16B43), (0x16B50, 0x16B59), (0x16B5B, 0x16B61), (0x16B63, 0x16B77),
    (0x16B7D, 0x16B8F), (0x16D40, 0x16D6C), (0x16D70, 0x16D79), (0x16E40, 0x16E96),
    (0x16F00, 0x16F4A), (0x16F4F, 0x16F87), (0x16F8F, 0x16F9F), (0x16FE0, 0x16FE1),
    (0x16FE3, 0x16FE4), (0x16FF0, 0x16FF1), (0x17000, 0x187F7), (0x18800, 0x18CD5),
    (0x18CFF, 0x18D08), (0x1AFF0, 0x1AFF3), (0x1AFF5, 0x1AFFB), (0x1AFFD, 0x1AFFE),
    (0x1B000, 0x1B122), (0x1B132, 0x1B132), (0x1B150, 0x1B152), (0x1B155, 0x1B155),
    (0x1B164, 0x1B167), (0x1B170, 0x1B2FB), (0x1BC00, 0x1BC6A), (0x1BC70, 0x1BC7C),
    (0x1BC80, 0x1BC88), (0x1BC90, 0x1BC99), (0x1BC9D, 0x1BC9E), (0x1CCF0, 0x1CCF9),
    (0x1CF00, 0x1CF2D), (0x1CF30, 0x1CF46), (0x1D165, 0x1D169), (0x1D16D, 0x1D172),
    (0x1D17B, 0x1D182), (0x1D185, 0x1D18B), (0x1D1AA, 0x1D1AD), (0x1D242, 0x1D244),
    (0x1D2C0, 0x1D2D3), (0x1D2E0, 0x1D2F3), (0x1D360, 0x1D378), (0x1D400, 0x1D454),
    (0x1D456, 0x1D49C), (0x1D49E, 0x1D49F), (0x1D4A2, 0x1D4A2), (0x1D4A5, 0x1D4A6),
    (0x1D4A9, 0x1D4AC), (0x1D4AE, 0x1D4B9), (0x1D4BB, 0x1D4BB), (0x1D4BD, 0x1D4C3),
    (0x1D4C5, 0x1D505), (0x1D507, 0x1D50A), (0x1D50D, 0x1D514), (0x1D516, 0x1D51C),
    (0x1D51E, 0x1D539), (0x1D53B, 0x1D53E), (0x1D540, 0x1D544), (0x1D546, 0x1D546),
    (0x1D54A, 0x1D550), (0x1D552, 0x1D6A5), (0x1D6A8, 0x1D6C0), (0x1D6C2, 0x1D6DA),
    (0x1D6DC, 0x1D6FA), (0x1D6FC, 0x1D714), (0x1D716, 0x1D734), (0x1D736, 0x1D74E),
    (0x1D750, 0x1D76E), (0x1D770, 0x1D788), (0x1D78A, 0x1D7A8), (0x1D7AA, 0x1D7C2),
    (0x1D7C4, 0x1D7CB), (0x1D7CE, 0x1D7FF), (0x1DA00, 0x1DA36), (0x1DA3B, 0x1DA6C),
    (0x1DA75, 0x1DA75), (0x1DA84, 0x1DA84), (0x1DA9B, 0x1DA9F), (0x1DAA1, 0x1DAAF),
    (0x1DF00, 0x1DF1E), (0x1DF25, 0x1DF2A), (0x1E000, 0x1E006), (0x1E008, 0x1E018),
    (0x1E01B, 0x1E021), (0x1E023, 0x1E024), (0x1E026, 0x1E02A), (0x1E030, 0x1E06D),
    (0x1E08F, 0x1E08F), (0x1E100, 0x1E12C), (0x1E130, 0x1E13D), (0x1E140, 0x1E149),
    (0x1E14E, 0x1E14E), (0x1E290, 0x1E2AE), (0x1E2C0, 0x1E2F9), (0x1E4D0, 0x1E4F9),
    (0x1E5D0, 0x1E5FA), (0x1E7E0, 0x1E7E6), (0x1E7E8, 0x1E7EB), (0x1E7ED, 0x1E7EE),
    (0x1E7F0, 0x1E7FE), (0x1E800, 0x1E8C4), (0x1E8C7, 0x1E8D6), (0x1E900, 0x1E94B),
    (0x1E950, 0x1E959), (0x1EC71, 0x1ECAB), (0x1ECAD, 0x1ECAF), (0x1ECB1, 0x1ECB4),
    (0x1ED01, 0x1ED2D), (0x1ED2F, 0x1ED3D), (0x1EE00, 0x1EE03), (0x1EE05, 0x1EE1F),
    (0x1EE21, 0x1EE22), (0x1EE24, 0x1EE24), (0x1EE27, 0x1EE27), (0x1EE29, 0x1EE32),
    (0x1EE34, 0x1EE37), (0x1EE39, 0x1EE39), (0x1EE3B, 0x1EE3B), (0x1EE42, 0x1EE42),
    (0x1EE47, 0x1EE47), (0x1EE49, 0x1EE49), (0x1EE4B, 0x1EE4B), (0x1EE4D, 0x1EE4F),
    (0x1EE51, 0x1EE52), (0x1EE54, 0x1EE54), (0x1EE57, 0x1EE57), (0x1EE59, 0x1EE59),
    (0x1EE5B, 0x1EE5B), (0x1EE5D, 0x1EE5D), (0x1EE5F, 0x1EE5F), (0x1EE61, 0x1EE62),
    (0x1EE64, 0x1EE64), (0x1EE67, 0x1EE6A), (0x1EE6C, 0x1EE72), (0x1EE74, 0x1EE77),
    (0x1EE79, 0x1EE7C), (0x1EE7E, 0x1EE7E), (0x1EE80, 0x1EE89), (0x1EE8B, 0x1EE9B),
    (0x1EEA1, 0x1EEA3), (0x1EEA5, 0x1EEA9), (0x1EEAB, 0x1EEBB), (0x1F100, 0x1F10C),
    (0x1FBF0, 0x1FBF9), (0x20000, 0x2A6DF), (0x2A700, 0x2B739), (0x2B740, 0x2B81D),
    (0x2B820, 0x2CEA1), (0x2CEB0, 0x2EBE0), (0x2EBF0, 0x2EE5D), (0x2F800, 0x2FA1D),
    (0x30000, 0x3134A), (0x31350, 0x323AF), (0xE0100, 0xE01EF),
];

/// Non-zero canonical combining classes, merged into ranges of equal class.
#[rustfmt::skip]
const CCC_RANGES: &[(u32, u32, u8)] = &[
    (0x0300, 0x0314, 230), (0x0315, 0x0315, 232), (0x0316, 0x0319, 220), (0x031A, 0x031A, 232),
    (0x031B, 0x031B, 216), (0x031C, 0x0320, 220), (0x0321, 0x0322, 202), (0x0323, 0x0326, 220),
    (0x0327, 0x0328, 202), (0x0329, 0x0333, 220), (0x0334, 0x0338, 1), (0x0339, 0x033C, 220),
    (0x033D, 0x0344, 230), (0x0345, 0x0345, 240), (0x0346, 0x0346, 230), (0x0347, 0x0349, 220),
    (0x034A, 0x034C, 230), (0x034D, 0x034E, 220), (0x0350, 0x0352, 230), (0x0353, 0x0356, 220),
    (0x0357, 0x0357, 230), (0x0358, 0x0358, 232), (0x0359, 0x035A, 220), (0x035B, 0x035B, 230),
    (0x035C, 0x035C, 233), (0x035D, 0x035E, 234), (0x035F, 0x035F, 233), (0x0360, 0x0361, 234),
    (0x0362, 0x0362, 233), (0x0363, 0x036F, 230), (0x0483, 0x0487, 230), (0x0591, 0x0591, 220),
    (0x0592, 0x0595, 230), (0x0596, 0x0596, 220), (0x0597, 0x0599, 230), (0x059A, 0x059A, 222),
    (0x059B, 0x059B, 220), (0x059C, 0x05A1, 230), (0x05A2, 0x05A7, 220), (0x05A8, 0x05A9, 230),
    (0x05AA, 0x05AA, 220), (0x05AB, 0x05AC, 230), (0x05AD, 0x05AD, 222), (0x05AE, 0x05AE, 228),
    (0x05AF, 0x05AF, 230), (0x05B0, 0x05B0, 10), (0x05B1, 0x05B1, 11), (0x05B2, 0x05B2, 12),
    (0x05B3, 0x05B3, 13), (0x05B4, 0x05B4, 14), (0x05B5, 0x05B5, 15), (0x05B6, 0x05B6, 16),
    (0x05B7, 0x05B7, 17), (0x05B8, 0x05B8, 18), (0x05B9, 0x05BA, 19), (0x05BB, 0x05BB, 20),
    (0x05BC, 0x05BC, 21), (0x05BD, 0x05BD, 22), (0x05BF, 0x05BF, 23), (0x05C1, 0x05C1, 24),
    (0x05C2, 0x05C2, 25), (0x05C4, 0x05C4, 230), (0x05C5, 0x05C5, 220), (0x05C7, 0x05C7, 18),
    (0x0610, 0x0617, 230), (0x0618, 0x0618, 30), (0x0619, 0x0619, 31), (0x061A, 0x061A, 32),
    (0x064B, 0x064B, 27), (0x064C, 0x064C, 28), (0x064D, 0x064D, 29), (0x064E, 0x064E, 30),
    (0x064F, 0x064F, 31), (0x0650, 0x0650, 32), (0x0651, 0x0651, 33), (0x0652, 0x0652, 34),
    (0x0653, 0x0654, 230), (0x0655, 0x0656, 220), (0x0657, 0x065B, 230), (0x065C, 0x065C, 220),
    (0x065D, 0x065E, 230), (0x065F, 0x065F, 220), (0x0670, 0x0670, 35), (0x06D6, 0x06DC, 230),
    (0x06DF, 0x06E2, 230), (0x06E3, 0x06E3, 220), (0x06E4, 0x06E4, 230), (0x06E7, 0x06E8, 230),
    (0x06EA, 0x06EA, 220), (0x06EB, 0x06EC, 230), (0x06ED, 0x06ED, 220), (0x0711, 0x0711, 36),
    (0x0730, 0x0730, 230), (0x0731, 0x0731, 220), (0x0732, 0x0733, 230), (0x0734, 0x0734, 220),
    (0x0735, 0x0736, 230), (0x0737, 0x0739, 220), (0x073A, 0x073A, 230), (0x073B, 0x073C, 220),
    (0x073D, 0x073D, 230), (0x073E, 0x073E, 220), (0x073F, 0x0741, 230), (0x0742, 0x0742, 220),
    (0x0743, 0x0743, 230), (0x0744, 0x0744, 220), (0x0745, 0x0745, 230), (0x0746, 0x0746, 220),
    (0x0747, 0x0747, 230), (0x0748, 0x0748, 220), (0x0749, 0x074A, 230), (0x07EB, 0x07F1, 230),
    (0x07F2, 0x07F2, 220), (0x07F3, 0x07F3, 230), (0x07FD, 0x07FD, 220), (0x0816, 0x0819, 230),
    (0x081B, 0x0823, 230), (0x0825, 0x0827, 230), (0x0829, 0x082D, 230), (0x0859, 0x085B, 220),
    (0x0897, 0x0898, 230), (0x0899, 0x089B, 220), (0x089C, 0x089F, 230), (0x08CA, 0x08CE, 230),
    (0x08CF, 0x08D3, 220), (0x08D4, 0x08E1, 230), (0x08E3, 0x08E3, 220), (0x08E4, 0x08E5, 230),
    (0x08E6, 0x08E6, 220), (0x08E7, 0x08E8, 230), (0x08E9, 0x08E9, 220), (0x08EA, 0x08EC, 230),
    (0x08ED, 0x08EF, 220), (0x08F0, 0x08F0, 27), (0x08F1, 0x08F1, 28), (0x08F2, 0x08F2, 29),
    (0x08F3, 0x08F5, 230), (0x08F6, 0x08F6, 220), (0x08F7, 0x08F8, 230), (0x08F9, 0x08FA, 220),
    (0x08FB, 0x08FF, 230), (0x093C, 0x093C, 7), (0x094D, 0x094D, 9), (0x0951, 0x0951, 230),
    (0x0952, 0x0952, 220), (0x0953, 0x0954, 230), (0x09BC, 0x09BC, 7), (0x09CD, 0x09CD, 9),
    (0x09FE, 0x09FE, 230), (0x0A3C, 0x0A3C, 7), (0x0A4D, 0x0A4D, 9), (0x0ABC, 0x0ABC, 7),
    (0x0ACD, 0x0ACD, 9), (0x0B3C, 0x0B3C, 7), (0x0B4D, 0x0B4D, 9), (0x0BCD, 0x0BCD, 9),
    (0x0C3C, 0x0C3C, 7), (0x0C4D, 0x0C4D, 9), (0x0C55, 0x0C55, 84), (0x0C56, 0x0C56, 91),
    (0x0CBC, 0x0CBC, 7), (0x0CCD, 0x0CCD, 9), (0x0D3B, 0x0D3C, 9), (0x0D4D, 0x0D4D, 9),
    (0x0DCA, 0x0DCA, 9), (0x0E38, 0x0E39, 103), (0x0E3A, 0x0E3A, 9), (0x0E48, 0x0E4B, 107),
    (0x0EB8, 0x0EB9, 118), (0x0EBA, 0x0EBA, 9), (0x0EC8, 0x0ECB, 122), (0x0F18, 0x0F19, 220),
    (0x0F35, 0x0F35, 220), (0x0F37, 0x0F37, 220), (0x0F39, 0x0F39, 216), (0x0F71, 0x0F71, 129),
    (0x0F72, 0x0F72, 130), (0x0F74, 0x0F74, 132), (0x0F7A, 0x0F7D, 130), (0x0F80, 0x0F80, 130),
    (0x0F82, 0x0F83, 230), (0x0F84, 0x0F84, 9), (0x0F86, 0x0F87, 230), (0x0FC6, 0x0FC6, 220),
    (0x1037, 0x1037, 7), (0x1039, 0x103A, 9), (0x108D, 0x108D, 220), (0x135D, 0x135F, 230),
    (0x1714, 0x1715, 9), (0x1734, 0x1734, 9), (0x17D2, 0x17D2, 9), (0x17DD, 0x17DD, 230),
    (0x18A9, 0x18A9, 228), (0x1939, 0x1939, 222), (0x193A, 0x193A, 230), (0x193B, 0x193B, 220),
    (0x1A17, 0x1A17, 230), (0x1A18, 0x1A18, 220), (0x1A60, 0x1A60, 9), (0x1A75, 0x1A7C, 230),
    (0x1A7F, 0x1A7F, 220), (0x1AB0, 0x1AB4, 230), (0x1AB5, 0x1ABA, 220), (0x1ABB, 0x1ABC, 230),
    (0x1ABD, 0x1ABD, 220), (0x1ABF, 0x1AC0, 220), (0x1AC1, 0x1AC2, 230), (0x1AC3, 0x1AC4, 220),
    (0x1AC5, 0x1AC9, 230), (0x1ACA, 0x1ACA, 220), (0x1ACB, 0x1ACE, 230), (0x1B34, 0x1B34, 7),
    (0x1B44, 0x1B44, 9), (0x1B6B, 0x1B6B, 230), (0x1B6C, 0x1B6C, 220), (0x1B6D, 0x1B73, 230),
    (0x1BAA, 0x1BAB, 9), (0x1BE6, 0x1BE6, 7), (0x1BF2, 0x1BF3, 9), (0x1C37, 0x1C37, 7),
    (0x1CD0, 0x1CD2, 230), (0x1CD4, 0x1CD4, 1), (0x1CD5, 0x1CD9, 220), (0x1CDA, 0x1CDB, 230),
    (0x1CDC, 0x1CDF, 220), (0x1CE0, 0x1CE0, 230), (0x1CE2, 0x1CE8, 1), (0x1CED, 0x1CED, 220),
    (0x1CF4, 0x1CF4, 230), (0x1CF8, 0x1CF9, 230), (0x1DC0, 0x1DC1, 230), (0x1DC2, 0x1DC2, 220),
    (0x1DC3, 0x1DC9, 230), (0x1DCA, 0x1DCA, 220), (0x1DCB, 0x1DCC, 230), (0x1DCD, 0x1DCD, 234),
    (0x1DCE, 0x1DCE, 214), (0x1DCF, 0x1DCF, 220), (0x1DD0, 0x1DD0, 202), (0x1DD1, 0x1DF5, 230),
    (0x1DF6, 0x1DF6, 232), (0x1DF7, 0x1DF8, 228), (0x1DF9, 0x1DF9, 220), (0x1DFA, 0x1DFA, 218),
    (0x1DFB, 0x1DFB, 230), (0x1DFC, 0x1DFC, 233), (0x1DFD, 0x1DFD, 220), (0x1DFE, 0x1DFE, 230),
    (0x1DFF, 0x1DFF, 220), (0x20D0, 0x20D1, 230), (0x20D2, 0x20D3, 1), (0x20D4, 0x20D7, 230),
    (0x20D8, 0x20DA, 1), (0x20DB, 0x20DC, 230), (0x20E1, 0x20E1, 230), (0x20E5, 0x20E6, 1),
    (0x20E7, 0x20E7, 230), (0x20E8, 0x20E8, 220), (0x20E9, 0x20E9, 230), (0x20EA, 0x20EB, 1),
    (0x20EC, 0x20EF, 220), (0x20F0, 0x20F0, 230), (0x2CEF, 0x2CF1, 230), (0x2D7F, 0x2D7F, 9),
    (0x2DE0, 0x2DFF, 230), (0x302A, 0x302A, 218), (0x302B, 0x302B, 228), (0x302C, 0x302C, 232),
    (0x302D, 0x302D, 222), (0x302E, 0x302F, 224), (0x3099, 0x309A, 8), (0xA66F, 0xA66F, 230),
    (0xA674, 0xA67D, 230), (0xA69E, 0xA69F, 230), (0xA6F0, 0xA6F1, 230), (0xA806, 0xA806, 9),
    (0xA82C, 0xA82C, 9), (0xA8C4, 0xA8C4, 9), (0xA8E0, 0xA8F1, 230), (0xA92B, 0xA92D, 220),
    (0xA953, 0xA953, 9), (0xA9B3, 0xA9B3, 7), (0xA9C0, 0xA9C0, 9), (0xAAB0, 0xAAB0, 230),
    (0xAAB2, 0xAAB3, 230), (0xAAB4, 0xAAB4, 220), (0xAAB7, 0xAAB8, 230), (0xAABE, 0xAABF, 230),
    (0xAAC1, 0xAAC1, 230), (0xAAF6, 0xAAF6, 9), (0xABED, 0xABED, 9), (0xFB1E, 0xFB1E, 26),
    (0xFE20, 0xFE26, 230), (0xFE27, 0xFE2D, 220), (0xFE2E, 0xFE2F, 230), (0x101FD, 0x101FD, 220),
    (0x102E0, 0x102E0, 220), (0x10376, 0x1037A, 230), (0x10A0D, 0x10A0D, 220), (0x10A0F, 0x10A0F, 230),
    (0x10A38, 0x10A38, 230), (0x10A39, 0x10A39, 1), (0x10A3A, 0x10A3A, 220), (0x10A3F, 0x10A3F, 9),
    (0x10AE5, 0x10AE5, 230), (0x10AE6, 0x10AE6, 220), (0x10D24, 0x10D27, 230), (0x10D69, 0x10D6D, 230),
    (0x10EAB, 0x10EAC, 230), (0x10EFD, 0x10EFF, 220), (0x10F46, 0x10F47, 220), (0x10F48, 0x10F4A, 230),
    (0x10F4B, 0x10F4B, 220), (0x10F4C, 0x10F4C, 230), (0x10F4D, 0x10F50, 220), (0x10F82, 0x10F82, 230),
    (0x10F83, 0x10F83, 220), (0x10F84, 0x10F84, 230), (0x10F85, 0x10F85, 220), (0x11046, 0x11046, 9),
    (0x11070, 0x11070, 9), (0x1107F, 0x1107F, 9), (0x110B9, 0x110B9, 9), (0x110BA, 0x110BA, 7),
    (0x11100, 0x11102, 230), (0x11133, 0x11134, 9), (0x11173, 0x11173, 7), (0x111C0, 0x111C0, 9),
    (0x111CA, 0x111CA, 7), (0x11235, 0x11235, 9), (0x11236, 0x11236, 7), (0x112E9, 0x112E9, 7),
    (0x112EA, 0x112EA, 9), (0x1133B, 0x1133C, 7), (0x1134D, 0x1134D, 9), (0x11366, 0x1136C, 230),
    (0x11370, 0x11374, 230), (0x113CE, 0x113D0, 9), (0x11442, 0x11442, 9), (0x11446, 0x11446, 7),
    (0x1145E, 0x1145E, 230), (0x114C2, 0x114C2, 9), (0x114C3, 0x114C3, 7), (0x115BF, 0x115BF, 9),
    (0x115C0, 0x115C0, 7), (0x1163F, 0x1163F, 9), (0x116B6, 0x116B6, 9), (0x116B7, 0x116B7, 7),
    (0x1172B, 0x1172B, 9), (0x11839, 0x11839, 9), (0x1183A, 0x1183A, 7), (0x1193D, 0x1193E, 9),
    (0x11943, 0x11943, 7), (0x119E0, 0x119E0, 9), (0x11A34, 0x11A34, 9), (0x11A47, 0x11A47, 9),
    (0x11A99, 0x11A99, 9), (0x11C3F, 0x11C3F, 9), (0x11D42, 0x11D42, 7), (0x11D44, 0x11D45, 9),
    (0x11D97, 0x11D97, 9), (0x11F41, 0x11F42, 9), (0x1612F, 0x1612F, 9), (0x16AF0, 0x16AF4, 1),
    (0x16B30, 0x16B36, 230), (0x16FF0, 0x16FF1, 6), (0x1BC9E, 0x1BC9E, 1), (0x1D165, 0x1D166, 216),
    (0x1D167, 0x1D169, 1), (0x1D16D, 0x1D16D, 226), (0x1D16E, 0x1D172, 216), (0x1D17B, 0x1D182, 220),
    (0x1D185, 0x1D189, 230), (0x1D18A, 0x1D18B, 220), (0x1D1AA, 0x1D1AD, 230), (0x1D242, 0x1D244, 230),
    (0x1E000, 0x1E006, 230), (0x1E008, 0x1E018, 230), (0x1E01B, 0x1E021, 230), (0x1E023, 0x1E024, 230),
    (0x1E026, 0x1E02A, 230), (0x1E08F, 0x1E08F, 230), (0x1E130, 0x1E136, 230), (0x1E2AE, 0x1E2AE, 230),
    (0x1E2EC, 0x1E2EF, 230), (0x1E4EC, 0x1E4ED, 232), (0x1E4EE, 0x1E4EE, 220), (0x1E4EF, 0x1E4EF, 230),
    (0x1E5EE, 0x1E5EE, 230), (0x1E5EF, 0x1E5EF, 220), (0x1E8D0, 0x1E8D6, 220), (0x1E944, 0x1E949, 230),
    (0x1E94A, 0x1E94A, 7),
];

/// Primary composites — base, combining mark, and the composed character — taken from the canonical
/// two-code-point decompositions of every character that NFC leaves composed. Hangul syllables are
/// absent: [`primary_composite`] forms them arithmetically.
#[rustfmt::skip]
const COMPOSE_PAIRS: &[(u32, u32, u32)] = &[
    (0x003C, 0x0338, 0x226E), (0x003D, 0x0338, 0x2260), (0x003E, 0x0338, 0x226F),
    (0x0041, 0x0300, 0x00C0), (0x0041, 0x0301, 0x00C1), (0x0041, 0x0302, 0x00C2),
    (0x0041, 0x0303, 0x00C3), (0x0041, 0x0304, 0x0100), (0x0041, 0x0306, 0x0102),
    (0x0041, 0x0307, 0x0226), (0x0041, 0x0308, 0x00C4), (0x0041, 0x0309, 0x1EA2),
    (0x0041, 0x030A, 0x00C5), (0x0041, 0x030C, 0x01CD), (0x0041, 0x030F, 0x0200),
    (0x0041, 0x0311, 0x0202), (0x0041, 0x0323, 0x1EA0), (0x0041, 0x0325, 0x1E00),
    (0x0041, 0x0328, 0x0104), (0x0042, 0x0307, 0x1E02), (0x0042, 0x0323, 0x1E04),
    (0x0042, 0x0331, 0x1E06), (0x0043, 0x0301, 0x0106), (0x0043, 0x0302, 0x0108),
    (0x0043, 0x0307, 0x010A), (0x0043, 0x030C, 0x010C), (0x0043, 0x0327, 0x00C7),
    (0x0044, 0x0307, 0x1E0A), (0x0044, 0x030C, 0x010E), (0x0044, 0x0323, 0x1E0C),
    (0x0044, 0x0327, 0x1E10), (0x0044, 0x032D, 0x1E12), (0x0044, 0x0331, 0x1E0E),
    (0x0045, 0x0300, 0x00C8), (0x0045, 0x0301, 0x00C9), (0x0045, 0x0302, 0x00CA),
    (0x0045, 0x0303, 0x1EBC), (0x0045, 0x0304, 0x0112), (0x0045, 0x0306, 0x0114),
    (0x0045, 0x0307, 0x0116), (0x0045, 0x0308, 0x00CB), (0x0045, 0x0309, 0x1EBA),
    (0x0045, 0x030C, 0x011A), (0x0045, 0x030F, 0x0204), (0x0045, 0x0311, 0x0206),
    (0x0045, 0x0323, 0x1EB8), (0x0045, 0x0327, 0x0228), (0x0045, 0x0328, 0x0118),
    (0x0045, 0x032D, 0x1E18), (0x0045, 0x0330, 0x1E1A), (0x0046, 0x0307, 0x1E1E),
    (0x0047, 0x0301, 0x01F4), (0x0047, 0x0302, 0x011C), (0x0047, 0x0304, 0x1E20),
    (0x0047, 0x0306, 0x011E), (0x0047, 0x0307, 0x0120), (0x0047, 0x030C, 0x01E6),
    (0x0047, 0x0327, 0x0122), (0x0048, 0x0302, 0x0124), (0x0048, 0x0307, 0x1E22),
    (0x0048, 0x0308, 0x1E26), (0x0048, 0x030C, 0x021E), (0x0048, 0x0323, 0x1E24),
    (0x0048, 0x0327, 0x1E28), (0x0048, 0x032E, 0x1E2A), (0x0049, 0x0300, 0x00CC),
    (0x0049, 0x0301, 0x00CD), (0x0049, 0x0302, 0x00CE), (0x0049, 0x0303, 0x0128),
    (0x0049, 0x0304, 0x012A), (0x0049, 0x0306, 0x012C), (0x0049, 0x0307, 0x0130),
    (0x0049, 0x0308, 0x00CF), (0x0049, 0x0309, 0x1EC8), (0x0049, 0x030C, 0x01CF),
    (0x0049, 0x030F, 0x0208), (0x0049, 0x0311, 0x020A), (0x0049, 0x0323, 0x1ECA),
    (0x0049, 0x0328, 0x012E), (0x0049, 0x0330, 0x1E2C), (0x004A, 0x0302, 0x0134),
    (0x004B, 0x0301, 0x1E30), (0x004B, 0x030C, 0x01E8), (0x004B, 0x0323, 0x1E32),
    (0x004B, 0x0327, 0x0136), (0x004B, 0x0331, 0x1E34), (0x004C, 0x0301, 0x0139),
    (0x004C, 0x030C, 0x013D), (0x004C, 0x0323, 0x1E36), (0x004C, 0x0327, 0x013B),
    (0x004C, 0x032D, 0x1E3C), (0x004C, 0x0331, 0x1E3A), (0x004D, 0x0301, 0x1E3E),
    (0x004D, 0x0307, 0x1E40), (0x004D, 0x0323, 0x1E42), (0x004E, 0x0300, 0x01F8),
    (0x004E, 0x0301, 0x0143), (0x004E, 0x0303, 0x00D1), (0x004E, 0x0307, 0x1E44),
    (0x004E, 0x030C, 0x0147), (0x004E, 0x0323, 0x1E46), (0x004E, 0x0327, 0x0145),
    (0x004E, 0x032D, 0x1E4A), (0x004E, 0x0331, 0x1E48), (0x004F, 0x0300, 0x00D2),
    (0x004F, 0x0301, 0x00D3), (0x004F, 0x0302, 0x00D4), (0x004F, 0x0303, 0x00D5),
    (0x004F, 0x0304, 0x014C), (0x004F, 0x0306, 0x014E), (0x004F, 0x0307, 0x022E),
    (0x004F, 0x0308, 0x00D6), (0x004F, 0x0309, 0x1ECE), (0x004F, 0x030B, 0x0150),
    (0x004F, 0x030C, 0x01D1), (0x004F, 0x030F, 0x020C), (0x004F, 0x0311, 0x020E),
    (0x004F, 0x031B, 0x01A0), (0x004F, 0x0323, 0x1ECC), (0x004F, 0x0328, 0x01EA),
    (0x0050, 0x0301, 0x1E54), (0x0050, 0x0307, 0x1E56), (0x0052, 0x0301, 0x0154),
    (0x0052, 0x0307, 0x1E58), (0x0052, 0x030C, 0x0158), (0x0052, 0x030F, 0x0210),
    (0x0052, 0x0311, 0x0212), (0x0052, 0x0323, 0x1E5A), (0x0052, 0x0327, 0x0156),
    (0x0052, 0x0331, 0x1E5E), (0x0053, 0x0301, 0x015A), (0x0053, 0x0302, 0x015C),
    (0x0053, 0x0307, 0x1E60), (0x0053, 0x030C, 0x0160), (0x0053, 0x0323, 0x1E62),
    (0x0053, 0x0326, 0x0218), (0x0053, 0x0327, 0x015E), (0x0054, 0x0307, 0x1E6A),
    (0x0054, 0x030C, 0x0164), (0x0054, 0x0323, 0x1E6C), (0x0054, 0x0326, 0x021A),
    (0x0054, 0x0327, 0x0162), (0x0054, 0x032D, 0x1E70), (0x0054, 0x0331, 0x1E6E),
    (0x0055, 0x0300, 0x00D9), (0x0055, 0x0301, 0x00DA), (0x0055, 0x0302, 0x00DB),
    (0x0055, 0x0303, 0x0168), (0x0055, 0x0304, 0x016A), (0x0055, 0x0306, 0x016C),
    (0x0055, 0x0308, 0x00DC), (0x0055, 0x0309, 0x1EE6), (0x0055, 0x030A, 0x016E),
    (0x0055, 0x030B, 0x0170), (0x0055, 0x030C, 0x01D3), (0x0055, 0x030F, 0x0214),
    (0x0055, 0x0311, 0x0216), (0x0055, 0x031B, 0x01AF), (0x0055, 0x0323, 0x1EE4),
    (0x0055, 0x0324, 0x1E72), (0x0055, 0x0328, 0x0172), (0x0055, 0x032D, 0x1E76),
    (0x0055, 0x0330, 0x1E74), (0x0056, 0x0303, 0x1E7C), (0x0056, 0x0323, 0x1E7E),
    (0x0057, 0x0300, 0x1E80), (0x0057, 0x0301, 0x1E82), (0x0057, 0x0302, 0x0174),
    (0x0057, 0x0307, 0x1E86), (0x0057, 0x0308, 0x1E84), (0x0057, 0x0323, 0x1E88),
    (0x0058, 0x0307, 0x1E8A), (0x0058, 0x0308, 0x1E8C), (0x0059, 0x0300, 0x1EF2),
    (0x0059, 0x0301, 0x00DD), (0x0059, 0x0302, 0x0176), (0x0059, 0x0303, 0x1EF8),
    (0x0059, 0x0304, 0x0232), (0x0059, 0x0307, 0x1E8E), (0x0059, 0x0308, 0x0178),
    (0x0059, 0x0309, 0x1EF6), (0x0059, 0x0323, 0x1EF4), (0x005A, 0x0301, 0x0179),
    (0x005A, 0x0302, 0x1E90), (0x005A, 0x0307, 0x017B), (0x005A, 0x030C, 0x017D),
    (0x005A, 0x0323, 0x1E92), (0x005A, 0x0331, 0x1E94), (0x0061, 0x0300, 0x00E0),
    (0x0061, 0x0301, 0x00E1), (0x0061, 0x0302, 0x00E2), (0x0061, 0x0303, 0x00E3),
    (0x0061, 0x0304, 0x0101), (0x0061, 0x0306, 0x0103), (0x0061, 0x0307, 0x0227),
    (0x0061, 0x0308, 0x00E4), (0x0061, 0x0309, 0x1EA3), (0x0061, 0x030A, 0x00E5),
    (0x0061, 0x030C, 0x01CE), (0x0061, 0x030F, 0x0201), (0x0061, 0x0311, 0x0203),
    (0x0061, 0x0323, 0x1EA1), (0x0061, 0x0325, 0x1E01), (0x0061, 0x0328, 0x0105),
    (0x0062, 0x0307, 0x1E03), (0x0062, 0x0323, 0x1E05), (0x0062, 0x0331, 0x1E07),
    (0x0063, 0x0301, 0x0107), (0x0063, 0x0302, 0x0109), (0x0063, 0x0307, 0x010B),
    (0x0063, 0x030C, 0x010D), (0x0063, 0x0327, 0x00E7), (0x0064, 0x0307, 0x1E0B),
    (0x0064, 0x030C, 0x010F), (0x0064, 0x0323, 0x1E0D), (0x0064, 0x0327, 0x1E11),
    (0x0064, 0x032D, 0x1E13), (0x0064, 0x0331, 0x1E0F), (0x0065, 0x0300, 0x00E8),
    (0x0065, 0x0301, 0x00E9), (0x0065, 0x0302, 0x00EA), (0x0065, 0x0303, 0x1EBD),
    (0x0065, 0x0304, 0x0113), (0x0065, 0x0306, 0x0115), (0x0065, 0x0307, 0x0117),
    (0x0065, 0x0308, 0x00EB), (0x0065, 0x0309, 0x1EBB), (0x0065, 0x030C, 0x011B),
    (0x0065, 0x030F, 0x0205), (0x0065, 0x0311, 0x0207), (0x0065, 0x0323, 0x1EB9),
    (0x0065, 0x0327, 0x0229), (0x0065, 0x0328, 0x0119), (0x0065, 0x032D, 0x1E19),
    (0x0065, 0x0330, 0x1E1B), (0x0066, 0x0307, 0x1E1F), (0x0067, 0x0301, 0x01F5),
    (0x0067, 0x0302, 0x011D), (0x0067, 0x0304, 0x1E21), (0x0067, 0x0306, 0x011F),
    (0x0067, 0x0307, 0x0121), (0x0067, 0x030C, 0x01E7), (0x0067, 0x0327, 0x0123),
    (0x0068, 0x0302, 0x0125), (0x0068, 0x0307, 0x1E23), (0x0068, 0x0308, 0x1E27),
    (0x0068, 0x030C, 0x021F), (0x0068, 0x0323, 0x1E25), (0x0068, 0x0327, 0x1E29),
    (0x0068, 0x032E, 0x1E2B), (0x0068, 0x0331, 0x1E96), (0x0069, 0x0300, 0x00EC),
    (0x0069, 0x0301, 0x00ED), (0x0069, 0x0302, 0x00EE), (0x0069, 0x0303, 0x0129),
    (0x0069, 0x0304, 0x012B), (0x0069, 0x0306, 0x012D), (0x0069, 0x0308, 0x00EF),
    (0x0069, 0x0309, 0x1EC9), (0x0069, 0x030C, 0x01D0), (0x0069, 0x030F, 0x0209),
    (0x0069, 0x0311, 0x020B), (0x0069, 0x0323, 0x1ECB), (0x0069, 0x0328, 0x012F),
    (0x0069, 0x0330, 0x1E2D), (0x006A, 0x0302, 0x0135), (0x006A, 0x030C, 0x01F0),
    (0x006B, 0x0301, 0x1E31), (0x006B, 0x030C, 0x01E9), (0x006B, 0x0323, 0x1E33),
    (0x006B, 0x0327, 0x0137), (0x006B, 0x0331, 0x1E35), (0x006C, 0x0301, 0x013A),
    (0x006C, 0x030C, 0x013E), (0x006C, 0x0323, 0x1E37), (0x006C, 0x0327, 0x013C),
    (0x006C, 0x032D, 0x1E3D), (0x006C, 0x0331, 0x1E3B), (0x006D, 0x0301, 0x1E3F),
    (0x006D, 0x0307, 0x1E41), (0x006D, 0x0323, 0x1E43), (0x006E, 0x0300, 0x01F9),
    (0x006E, 0x0301, 0x0144), (0x006E, 0x0303, 0x00F1), (0x006E, 0x0307, 0x1E45),
    (0x006E, 0x030C, 0x0148), (0x006E, 0x0323, 0x1E47), (0x006E, 0x0327, 0x0146),
    (0x006E, 0x032D, 0x1E4B), (0x006E, 0x0331, 0x1E49), (0x006F, 0x0300, 0x00F2),
    (0x006F, 0x0301, 0x00F3), (0x006F, 0x0302, 0x00F4), (0x006F, 0x0303, 0x00F5),
    (0x006F, 0x0304, 0x014D), (0x006F, 0x0306, 0x014F), (0x006F, 0x0307, 0x022F),
    (0x006F, 0x0308, 0x00F6), (0x006F, 0x0309, 0x1ECF), (0x006F, 0x030B, 0x0151),
    (0x006F, 0x030C, 0x01D2), (0x006F, 0x030F, 0x020D), (0x006F, 0x0311, 0x020F),
    (0x006F, 0x031B, 0x01A1), (0x006F, 0x0323, 0x1ECD), (0x006F, 0x0328, 0x01EB),
    (0x0070, 0x0301, 0x1E55), (0x0070, 0x0307, 0x1E57), (0x0072, 0x0301, 0x0155),
    (0x0072, 0x0307, 0x1E59), (0x0072, 0x030C, 0x0159), (0x0072, 0x030F, 0x0211),
    (0x0072, 0x0311, 0x0213), (0x0072, 0x0323, 0x1E5B), (0x0072, 0x0327, 0x0157),
    (0x0072, 0x0331, 0x1E5F), (0x0073, 0x0301, 0x015B), (0x0073, 0x0302, 0x015D),
    (0x0073, 0x0307, 0x1E61), (0x0073, 0x030C, 0x0161), (0x0073, 0x0323, 0x1E63),
    (0x0073, 0x0326, 0x0219), (0x0073, 0x0327, 0x015F), (0x0074, 0x0307, 0x1E6B),
    (0x0074, 0x0308, 0x1E97), (0x0074, 0x030C, 0x0165), (0x0074, 0x0323, 0x1E6D),
    (0x0074, 0x0326, 0x021B), (0x0074, 0x0327, 0x0163), (0x0074, 0x032D, 0x1E71),
    (0x0074, 0x0331, 0x1E6F), (0x0075, 0x0300, 0x00F9), (0x0075, 0x0301, 0x00FA),
    (0x0075, 0x0302, 0x00FB), (0x0075, 0x0303, 0x0169), (0x0075, 0x0304, 0x016B),
    (0x0075, 0x0306, 0x016D), (0x0075, 0x0308, 0x00FC), (0x0075, 0x0309, 0x1EE7),
    (0x0075, 0x030A, 0x016F), (0x0075, 0x030B, 0x0171), (0x0075, 0x030C, 0x01D4),
    (0x0075, 0x030F, 0x0215), (0x0075, 0x0311, 0x0217), (0x0075, 0x031B, 0x01B0),
    (0x0075, 0x0323, 0x1EE5), (0x0075, 0x0324, 0x1E73), (0x0075, 0x0328, 0x0173),
    (0x0075, 0x032D, 0x1E77), (0x0075, 0x0330, 0x1E75), (0x0076, 0x0303, 0x1E7D),
    (0x0076, 0x0323, 0x1E7F), (0x0077, 0x0300, 0x1E81), (0x0077, 0x0301, 0x1E83),
    (0x0077, 0x0302, 0x0175), (0x0077, 0x0307, 0x1E87), (0x0077, 0x0308, 0x1E85),
    (0x0077, 0x030A, 0x1E98), (0x0077, 0x0323, 0x1E89), (0x0078, 0x0307, 0x1E8B),
    (0x0078, 0x0308, 0x1E8D), (0x0079, 0x0300, 0x1EF3), (0x0079, 0x0301, 0x00FD),
    (0x0079, 0x0302, 0x0177), (0x0079, 0x0303, 0x1EF9), (0x0079, 0x0304, 0x0233),
    (0x0079, 0x0307, 0x1E8F), (0x0079, 0x0308, 0x00FF), (0x0079, 0x0309, 0x1EF7),
    (0x0079, 0x030A, 0x1E99), (0x0079, 0x0323, 0x1EF5), (0x007A, 0x0301, 0x017A),
    (0x007A, 0x0302, 0x1E91), (0x007A, 0x0307, 0x017C), (0x007A, 0x030C, 0x017E),
    (0x007A, 0x0323, 0x1E93), (0x007A, 0x0331, 0x1E95), (0x00A8, 0x0300, 0x1FED),
    (0x00A8, 0x0301, 0x0385), (0x00A8, 0x0342, 0x1FC1), (0x00C6, 0x0301, 0x01FC),
    (0x00C6, 0x0304, 0x01E2), (0x00D8, 0x0301, 0x01FE), (0x00E6, 0x0301, 0x01FD),
    (0x00E6, 0x0304, 0x01E3), (0x00F8, 0x0301, 0x01FF), (0x017F, 0x0307, 0x1E9B),
    (0x01B7, 0x030C, 0x01EE), (0x0292, 0x030C, 0x01EF), (0x0391, 0x0300, 0x1FBA),
    (0x0391, 0x0301, 0x0386), (0x0391, 0x0304, 0x1FB9), (0x0391, 0x0306, 0x1FB8),
    (0x0391, 0x0313, 0x1F08), (0x0391, 0x0314, 0x1F09), (0x0391, 0x0345, 0x1FBC),
    (0x0395, 0x0300, 0x1FC8), (0x0395, 0x0301, 0x0388), (0x0395, 0x0313, 0x1F18),
    (0x0395, 0x0314, 0x1F19), (0x0397, 0x0300, 0x1FCA), (0x0397, 0x0301, 0x0389),
    (0x0397, 0x0313, 0x1F28), (0x0397, 0x0314, 0x1F29), (0x0397, 0x0345, 0x1FCC),
    (0x0399, 0x0300, 0x1FDA), (0x0399, 0x0301, 0x038A), (0x0399, 0x0304, 0x1FD9),
    (0x0399, 0x0306, 0x1FD8), (0x0399, 0x0308, 0x03AA), (0x0399, 0x0313, 0x1F38),
    (0x0399, 0x0314, 0x1F39), (0x039F, 0x0300, 0x1FF8), (0x039F, 0x0301, 0x038C),
    (0x039F, 0x0313, 0x1F48), (0x039F, 0x0314, 0x1F49), (0x03A1, 0x0314, 0x1FEC),
    (0x03A5, 0x0300, 0x1FEA), (0x03A5, 0x0301, 0x038E), (0x03A5, 0x0304, 0x1FE9),
    (0x03A5, 0x0306, 0x1FE8), (0x03A5, 0x0308, 0x03AB), (0x03A5, 0x0314, 0x1F59),
    (0x03A9, 0x0300, 0x1FFA), (0x03A9, 0x0301, 0x038F), (0x03A9, 0x0313, 0x1F68),
    (0x03A9, 0x0314, 0x1F69), (0x03A9, 0x0345, 0x1FFC), (0x03B1, 0x0300, 0x1F70),
    (0x03B1, 0x0301, 0x03AC), (0x03B1, 0x0304, 0x1FB1), (0x03B1, 0x0306, 0x1FB0),
    (0x03B1, 0x0313, 0x1F00), (0x03B1, 0x0314, 0x1F01), (0x03B1, 0x0342, 0x1FB6),
    (0x03B1, 0x0345, 0x1FB3), (0x03B5, 0x0300, 0x1F72), (0x03B5, 0x0301, 0x03AD),
    (0x03B5, 0x0313, 0x1F10), (0x03B5, 0x0314, 0x1F11), (0x03B7, 0x0300, 0x1F74),
    (0x03B7, 0x0301, 0x03AE), (0x03B7, 0x0313, 0x1F20), (0x03B7, 0x0314, 0x1F21),
    (0x03B7, 0x0342, 0x1FC6), (0x03B7, 0x0345, 0x1FC3), (0x03B9, 0x0300, 0x1F76),
    (0x03B9, 0x0301, 0x03AF), (0x03B9, 0x0304, 0x1FD1), (0x03B9, 0x0306, 0x1FD0),
    (0x03B9, 0x0308, 0x03CA), (0x03B9, 0x0313, 0x1F30), (0x03B9, 0x0314, 0x1F31),
    (0x03B9, 0x0342, 0x1FD6), (0x03BF, 0x0300, 0x1F78), (0x03BF, 0x0301, 0x03CC),
    (0x03BF, 0x0313, 0x1F40), (0x03BF, 0x0314, 0x1F41), (0x03C1, 0x0313, 0x1FE4),
    (0x03C1, 0x0314, 0x1FE5), (0x03C5, 0x0300, 0x1F7A), (0x03C5, 0x0301, 0x03CD),
    (0x03C5, 0x0304, 0x1FE1), (0x03C5, 0x0306, 0x1FE0), (0x03C5, 0x0308, 0x03CB),
    (0x03C5, 0x0313, 0x1F50), (0x03C5, 0x0314, 0x1F51), (0x03C5, 0x0342, 0x1FE6),
    (0x03C9, 0x0300, 0x1F7C), (0x03C9, 0x0301, 0x03CE), (0x03C9, 0x0313, 0x1F60),
    (0x03C9, 0x0314, 0x1F61), (0x03C9, 0x0342, 0x1FF6), (0x03C9, 0x0345, 0x1FF3),
    (0x03D2, 0x0301, 0x03D3), (0x03D2, 0x0308, 0x03D4), (0x0406, 0x0308, 0x0407),
    (0x0410, 0x0306, 0x04D0), (0x0410, 0x0308, 0x04D2), (0x0413, 0x0301, 0x0403),
    (0x0415, 0x0300, 0x0400), (0x0415, 0x0306, 0x04D6), (0x0415, 0x0308, 0x0401),
    (0x0416, 0x0306, 0x04C1), (0x0416, 0x0308, 0x04DC), (0x0417, 0x0308, 0x04DE),
    (0x0418, 0x0300, 0x040D), (0x0418, 0x0304, 0x04E2), (0x0418, 0x0306, 0x0419),
    (0x0418, 0x0308, 0x04E4), (0x041A, 0x0301, 0x040C), (0x041E, 0x0308, 0x04E6),
    (0x0423, 0x0304, 0x04EE), (0x0423, 0x0306, 0x040E), (0x0423, 0x0308, 0x04F0),
    (0x0423, 0x030B, 0x04F2), (0x0427, 0x0308, 0x04F4), (0x042B, 0x0308, 0x04F8),
    (0x042D, 0x0308, 0x04EC), (0x0430, 0x0306, 0x04D1), (0x0430, 0x0308, 0x04D3),
    (0x0433, 0x0301, 0x0453), (0x0435, 0x0300, 0x0450), (0x0435, 0x0306, 0x04D7),
    (0x0435, 0x0308, 0x0451), (0x0436, 0x0306, 0x04C2), (0x0436, 0x0308, 0x04DD),
    (0x0437, 0x0308, 0x04DF), (0x0438, 0x0300, 0x045D), (0x0438, 0x0304, 0x04E3),
    (0x0438, 0x0306, 0x0439), (0x0438, 0x0308, 0x04E5), (0x043A, 0x0301, 0x045C),
    (0x043E, 0x0308, 0x04E7), (0x0443, 0x0304, 0x04EF), (0x0443, 0x0306, 0x045E),
    (0x0443, 0x0308, 0x04F1), (0x0443, 0x030B, 0x04F3), (0x0447, 0x0308, 0x04F5),
    (0x044B, 0x0308, 0x04F9), (0x044D, 0x0308, 0x04ED), (0x0456, 0x0308, 0x0457),
    (0x0474, 0x030F, 0x0476), (0x0475, 0x030F, 0x0477), (0x04D8, 0x0308, 0x04DA),
    (0x04D9, 0x0308, 0x04DB), (0x04E8, 0x0308, 0x04EA), (0x04E9, 0x0308, 0x04EB),
    (0x0627, 0x0653, 0x0622), (0x0627, 0x0654, 0x0623), (0x0627, 0x0655, 0x0625),
    (0x0648, 0x0654, 0x0624), (0x064A, 0x0654, 0x0626), (0x06C1, 0x0654, 0x06C2),
    (0x06D2, 0x0654, 0x06D3), (0x06D5, 0x0654, 0x06C0), (0x0928, 0x093C, 0x0929),
    (0x0930, 0x093C, 0x0931), (0x0933, 0x093C, 0x0934), (0x09C7, 0x09BE, 0x09CB),
    (0x09C7, 0x09D7, 0x09CC), (0x0B47, 0x0B3E, 0x0B4B), (0x0B47, 0x0B56, 0x0B48),
    (0x0B47, 0x0B57, 0x0B4C), (0x0B92, 0x0BD7, 0x0B94), (0x0BC6, 0x0BBE, 0x0BCA),
    (0x0BC6, 0x0BD7, 0x0BCC), (0x0BC7, 0x0BBE, 0x0BCB), (0x0C46, 0x0C56, 0x0C48),
    (0x0CBF, 0x0CD5, 0x0CC0), (0x0CC6, 0x0CC2, 0x0CCA), (0x0CC6, 0x0CD5, 0x0CC7),
    (0x0CC6, 0x0CD6, 0x0CC8), (0x0D46, 0x0D3E, 0x0D4A), (0x0D46, 0x0D57, 0x0D4C),
    (0x0D47, 0x0D3E, 0x0D4B), (0x0DD9, 0x0DCA, 0x0DDA), (0x0DD9, 0x0DCF, 0x0DDC),
    (0x0DD9, 0x0DDF, 0x0DDE), (0x1025, 0x102E, 0x1026), (0x1100, 0x1161, 0xAC00),
    (0x1100, 0x1162, 0xAC1C), (0x1100, 0x1163, 0xAC38), (0x1100, 0x1164, 0xAC54),
    (0x1100, 0x1165, 0xAC70), (0x1100, 0x1166, 0xAC8C), (0x1100, 0x1167, 0xACA8),
    (0x1100, 0x1168, 0xACC4), (0x1100, 0x1169, 0xACE0), (0x1100, 0x116A, 0xACFC),
    (0x1100, 0x116B, 0xAD18), (0x1100, 0x116C, 0xAD34), (0x1100, 0x116D, 0xAD50),
    (0x1100, 0x116E, 0xAD6C), (0x1100, 0x116F, 0xAD88), (0x1100, 0x1170, 0xADA4),
    (0x1100, 0x1171, 0xADC0), (0x1100, 0x1172, 0xADDC), (0x1100, 0x1173, 0xADF8),
    (0x1100, 0x1174, 0xAE14), (0x1100, 0x1175, 0xAE30), (0x1101, 0x1161, 0xAE4C),
    (0x1101, 0x1162, 0xAE68), (0x1101, 0x1163, 0xAE84), (0x1101, 0x1164, 0xAEA0),
    (0x1101, 0x1165, 0xAEBC), (0x1101, 0x1166, 0xAED8), (0x1101, 0x1167, 0xAEF4),
    (0x1101, 0x1168, 0xAF10), (0x1101, 0x1169, 0xAF2C), (0x1101, 0x116A, 0xAF48),
    (0x1101, 0x116B, 0xAF64), (0x1101, 0x116C, 0xAF80), (0x1101, 0x116D, 0xAF9C),
    (0x1101, 0x116E, 0xAFB8), (0x1101, 0x116F, 0xAFD4), (0x1101, 0x1170, 0xAFF0),
    (0x1101, 0x1171, 0xB00C), (0x1101, 0x1172, 0xB028), (0x1101, 0x1173, 0xB044),
    (0x1101, 0x1174, 0xB060), (0x1101, 0x1175, 0xB07C), (0x1102, 0x1161, 0xB098),
    (0x1102, 0x1162, 0xB0B4), (0x1102, 0x1163, 0xB0D0), (0x1102, 0x1164, 0xB0EC),
    (0x1102, 0x1165, 0xB108), (0x1102, 0x1166, 0xB124), (0x1102, 0x1167, 0xB140),
    (0x1102, 0x1168, 0xB15C), (0x1102, 0x1169, 0xB178), (0x1102, 0x116A, 0xB194),
    (0x1102, 0x116B, 0xB1B0), (0x1102, 0x116C, 0xB1CC), (0x1102, 0x116D, 0xB1E8),
    (0x1102, 0x116E, 0xB204), (0x1102, 0x116F, 0xB220), (0x1102, 0x1170, 0xB23C),
    (0x1102, 0x1171, 0xB258), (0x1102, 0x1172, 0xB274), (0x1102, 0x1173, 0xB290),
    (0x1102, 0x1174, 0xB2AC), (0x1102, 0x1175, 0xB2C8), (0x1103, 0x1161, 0xB2E4),
    (0x1103, 0x1162, 0xB300), (0x1103, 0x1163, 0xB31C), (0x1103, 0x1164, 0xB338),
    (0x1103, 0x1165, 0xB354), (0x1103, 0x1166, 0xB370), (0x1103, 0x1167, 0xB38C),
    (0x1103, 0x1168, 0xB3A8), (0x1103, 0x1169, 0xB3C4), (0x1103, 0x116A, 0xB3E0),
    (0x1103, 0x116B, 0xB3FC), (0x1103, 0x116C, 0xB418), (0x1103, 0x116D, 0xB434),
    (0x1103, 0x116E, 0xB450), (0x1103, 0x116F, 0xB46C), (0x1103, 0x1170, 0xB488),
    (0x1103, 0x1171, 0xB4A4), (0x1103, 0x1172, 0xB4C0), (0x1103, 0x1173, 0xB4DC),
    (0x1103, 0x1174, 0xB4F8), (0x1103, 0x1175, 0xB514), (0x1104, 0x1161, 0xB530),
    (0x1104, 0x1162, 0xB54C), (0x1104, 0x1163, 0xB568), (0x1104, 0x1164, 0xB584),
    (0x1104, 0x1165, 0xB5A0), (0x1104, 0x1166, 0xB5BC), (0x1104, 0x1167, 0xB5D8),
    (0x1104, 0x1168, 0xB5F4), (0x1104, 0x1169, 0xB610), (0x1104, 0x116A, 0xB62C),
    (0x1104, 0x116B, 0xB648), (0x1104, 0x116C, 0xB664), (0x1104, 0x116D, 0xB680),
    (0x1104, 0x116E, 0xB69C), (0x1104, 0x116F, 0xB6B8), (0x1104, 0x1170, 0xB6D4),
    (0x1104, 0x1171, 0xB6F0), (0x1104, 0x1172, 0xB70C), (0x1104, 0x1173, 0xB728),
    (0x1104, 0x1174, 0xB744), (0x1104, 0x1175, 0xB760), (0x1105, 0x1161, 0xB77C),
    (0x1105, 0x1162, 0xB798), (0x1105, 0x1163, 0xB7B4), (0x1105, 0x1164, 0xB7D0),
    (0x1105, 0x1165, 0xB7EC), (0x1105, 0x1166, 0xB808), (0x1105, 0x1167, 0xB824),
    (0x1105, 0x1168, 0xB840), (0x1105, 0x1169, 0xB85C), (0x1105, 0x116A, 0xB878),
    (0x1105, 0x116B, 0xB894), (0x1105, 0x116C, 0xB8B0), (0x1105, 0x116D, 0xB8CC),
    (0x1105, 0x116E, 0xB8E8), (0x1105, 0x116F, 0xB904), (0x1105, 0x1170, 0xB920),
    (0x1105, 0x1171, 0xB93C), (0x1105, 0x1172, 0xB958), (0x1105, 0x1173, 0xB974),
    (0x1105, 0x1174, 0xB990), (0x1105, 0x1175, 0xB9AC), (0x1106, 0x1161, 0xB9C8),
    (0x1106, 0x1162, 0xB9E4), (0x1106, 0x1163, 0xBA00), (0x1106, 0x1164, 0xBA1C),
    (0x1106, 0x1165, 0xBA38), (0x1106, 0x1166, 0xBA54), (0x1106, 0x1167, 0xBA70),
    (0x1106, 0x1168, 0xBA8C), (0x1106, 0x1169, 0xBAA8), (0x1106, 0x116A, 0xBAC4),
    (0x1106, 0x116B, 0xBAE0), (0x1106, 0x116C, 0xBAFC), (0x1106, 0x116D, 0xBB18),
    (0x1106, 0x116E, 0xBB34), (0x1106, 0x116F, 0xBB50), (0x1106, 0x1170, 0xBB6C),
    (0x1106, 0x1171, 0xBB88), (0x1106, 0x1172, 0xBBA4), (0x1106, 0x1173, 0xBBC0),
    (0x1106, 0x1174, 0xBBDC), (0x1106, 0x1175, 0xBBF8), (0x1107, 0x1161, 0xBC14),
    (0x1107, 0x1162, 0xBC30), (0x1107, 0x1163, 0xBC4C), (0x1107, 0x1164, 0xBC68),
    (0x1107, 0x1165, 0xBC84), (0x1107, 0x1166, 0xBCA0), (0x1107, 0x1167, 0xBCBC),
    (0x1107, 0x1168, 0xBCD8), (0x1107, 0x1169, 0xBCF4), (0x1107, 0x116A, 0xBD10),
    (0x1107, 0x116B, 0xBD2C), (0x1107, 0x116C, 0xBD48), (0x1107, 0x116D, 0xBD64),
    (0x1107, 0x116E, 0xBD80), (0x1107, 0x116F, 0xBD9C), (0x1107, 0x1170, 0xBDB8),
    (0x1107, 0x1171, 0xBDD4), (0x1107, 0x1172, 0xBDF0), (0x1107, 0x1173, 0xBE0C),
    (0x1107, 0x1174, 0xBE28), (0x1107, 0x1175, 0xBE44), (0x1108, 0x1161, 0xBE60),
    (0x1108, 0x1162, 0xBE7C), (0x1108, 0x1163, 0xBE98), (0x1108, 0x1164, 0xBEB4),
    (0x1108, 0x1165, 0xBED0), (0x1108, 0x1166, 0xBEEC), (0x1108, 0x1167, 0xBF08),
    (0x1108, 0x1168, 0xBF24), (0x1108, 0x1169, 0xBF40), (0x1108, 0x116A, 0xBF5C),
    (0x1108, 0x116B, 0xBF78), (0x1108, 0x116C, 0xBF94), (0x1108, 0x116D, 0xBFB0),
    (0x1108, 0x116E, 0xBFCC), (0x1108, 0x116F, 0xBFE8), (0x1108, 0x1170, 0xC004),
    (0x1108, 0x1171, 0xC020), (0x1108, 0x1172, 0xC03C), (0x1108, 0x1173, 0xC058),
    (0x1108, 0x1174, 0xC074), (0x1108, 0x1175, 0xC090), (0x1109, 0x1161, 0xC0AC),
    (0x1109, 0x1162, 0xC0C8), (0x1109, 0x1163, 0xC0E4), (0x1109, 0x1164, 0xC100),
    (0x1109, 0x1165, 0xC11C), (0x1109, 0x1166, 0xC138), (0x1109, 0x1167, 0xC154),
    (0x1109, 0x1168, 0xC170), (0x1109, 0x1169, 0xC18C), (0x1109, 0x116A, 0xC1A8),
    (0x1109, 0x116B, 0xC1C4), (0x1109, 0x116C, 0xC1E0), (0x1109, 0x116D, 0xC1FC),
    (0x1109, 0x116E, 0xC218), (0x1109, 0x116F, 0xC234), (0x1109, 0x1170, 0xC250),
    (0x1109, 0x1171, 0xC26C), (0x1109, 0x1172, 0xC288), (0x1109, 0x1173, 0xC2A4),
    (0x1109, 0x1174, 0xC2C0), (0x1109, 0x1175, 0xC2DC), (0x110A, 0x1161, 0xC2F8),
    (0x110A, 0x1162, 0xC314), (0x110A, 0x1163, 0xC330), (0x110A, 0x1164, 0xC34C),
    (0x110A, 0x1165, 0xC368), (0x110A, 0x1166, 0xC384), (0x110A, 0x1167, 0xC3A0),
    (0x110A, 0x1168, 0xC3BC), (0x110A, 0x1169, 0xC3D8), (0x110A, 0x116A, 0xC3F4),
    (0x110A, 0x116B, 0xC410), (0x110A, 0x116C, 0xC42C), (0x110A, 0x116D, 0xC448),
    (0x110A, 0x116E, 0xC464), (0x110A, 0x116F, 0xC480), (0x110A, 0x1170, 0xC49C),
    (0x110A, 0x1171, 0xC4B8), (0x110A, 0x1172, 0xC4D4), (0x110A, 0x1173, 0xC4F0),
    (0x110A, 0x1174, 0xC50C), (0x110A, 0x1175, 0xC528), (0x110B, 0x1161, 0xC544),
    (0x110B, 0x1162, 0xC560), (0x110B, 0x1163, 0xC57C), (0x110B, 0x1164, 0xC598),
    (0x110B, 0x1165, 0xC5B4), (0x110B, 0x1166, 0xC5D0), (0x110B, 0x1167, 0xC5EC),
    (0x110B, 0x1168, 0xC608), (0x110B, 0x1169, 0xC624), (0x110B, 0x116A, 0xC640),
    (0x110B, 0x116B, 0xC65C), (0x110B, 0x116C, 0xC678), (0x110B, 0x116D, 0xC694),
    (0x110B, 0x116E, 0xC6B0), (0x110B, 0x116F, 0xC6CC), (0x110B, 0x1170, 0xC6E8),
    (0x110B, 0x1171, 0xC704), (0x110B, 0x1172, 0xC720), (0x110B, 0x1173, 0xC73C),
    (0x110B, 0x1174, 0xC758), (0x110B, 0x1175, 0xC774), (0x110C, 0x1161, 0xC790),
    (0x110C, 0x1162, 0xC7AC), (0x110C, 0x1163, 0xC7C8), (0x110C, 0x1164, 0xC7E4),
    (0x110C, 0x1165, 0xC800), (0x110C, 0x1166, 0xC81C), (0x110C, 0x1167, 0xC838),
    (0x110C, 0x1168, 0xC854), (0x110C, 0x1169, 0xC870), (0x110C, 0x116A, 0xC88C),
    (0x110C, 0x116B, 0xC8A8), (0x110C, 0x116C, 0xC8C4), (0x110C, 0x116D, 0xC8E0),
    (0x110C, 0x116E, 0xC8FC), (0x110C, 0x116F, 0xC918), (0x110C, 0x1170, 0xC934),
    (0x110C, 0x1171, 0xC950), (0x110C, 0x1172, 0xC96C), (0x110C, 0x1173, 0xC988),
    (0x110C, 0x1174, 0xC9A4), (0x110C, 0x1175, 0xC9C0), (0x110D, 0x1161, 0xC9DC),
    (0x110D, 0x1162, 0xC9F8), (0x110D, 0x1163, 0xCA14), (0x110D, 0x1164, 0xCA30),
    (0x110D, 0x1165, 0xCA4C), (0x110D, 0x1166, 0xCA68), (0x110D, 0x1167, 0xCA84),
    (0x110D, 0x1168, 0xCAA0), (0x110D, 0x1169, 0xCABC), (0x110D, 0x116A, 0xCAD8),
    (0x110D, 0x116B, 0xCAF4), (0x110D, 0x116C, 0xCB10), (0x110D, 0x116D, 0xCB2C),
    (0x110D, 0x116E, 0xCB48), (0x110D, 0x116F, 0xCB64), (0x110D, 0x1170, 0xCB80),
    (0x110D, 0x1171, 0xCB9C), (0x110D, 0x1172, 0xCBB8), (0x110D, 0x1173, 0xCBD4),
    (0x110D, 0x1174, 0xCBF0), (0x110D, 0x1175, 0xCC0C), (0x110E, 0x1161, 0xCC28),
    (0x110E, 0x1162, 0xCC44), (0x110E, 0x1163, 0xCC60), (0x110E, 0x1164, 0xCC7C),
    (0x110E, 0x1165, 0xCC98), (0x110E, 0x1166, 0xCCB4), (0x110E, 0x1167, 0xCCD0),
    (0x110E, 0x1168, 0xCCEC), (0x110E, 0x1169, 0xCD08), (0x110E, 0x116A, 0xCD24),
    (0x110E, 0x116B, 0xCD40), (0x110E, 0x116C, 0xCD5C), (0x110E, 0x116D, 0xCD78),
    (0x110E, 0x116E, 0xCD94), (0x110E, 0x116F, 0xCDB0), (0x110E, 0x1170, 0xCDCC),
    (0x110E, 0x1171, 0xCDE8), (0x110E, 0x1172, 0xCE04), (0x110E, 0x1173, 0xCE20),
    (0x110E, 0x1174, 0xCE3C), (0x110E, 0x1175, 0xCE58), (0x110F, 0x1161, 0xCE74),
    (0x110F, 0x1162, 0xCE90), (0x110F, 0x1163, 0xCEAC), (0x110F, 0x1164, 0xCEC8),
    (0x110F, 0x1165, 0xCEE4), (0x110F, 0x1166, 0xCF00), (0x110F, 0x1167, 0xCF1C),
    (0x110F, 0x1168, 0xCF38), (0x110F, 0x1169, 0xCF54), (0x110F, 0x116A, 0xCF70),
    (0x110F, 0x116B, 0xCF8C), (0x110F, 0x116C, 0xCFA8), (0x110F, 0x116D, 0xCFC4),
    (0x110F, 0x116E, 0xCFE0), (0x110F, 0x116F, 0xCFFC), (0x110F, 0x1170, 0xD018),
    (0x110F, 0x1171, 0xD034), (0x110F, 0x1172, 0xD050), (0x110F, 0x1173, 0xD06C),
    (0x110F, 0x1174, 0xD088), (0x110F, 0x1175, 0xD0A4), (0x1110, 0x1161, 0xD0C0),
    (0x1110, 0x1162, 0xD0DC), (0x1110, 0x1163, 0xD0F8), (0x1110, 0x1164, 0xD114),
    (0x1110, 0x1165, 0xD130), (0x1110, 0x1166, 0xD14C), (0x1110, 0x1167, 0xD168),
    (0x1110, 0x1168, 0xD184), (0x1110, 0x1169, 0xD1A0), (0x1110, 0x116A, 0xD1BC),
    (0x1110, 0x116B, 0xD1D8), (0x1110, 0x116C, 0xD1F4), (0x1110, 0x116D, 0xD210),
    (0x1110, 0x116E, 0xD22C), (0x1110, 0x116F, 0xD248), (0x1110, 0x1170, 0xD264),
    (0x1110, 0x1171, 0xD280), (0x1110, 0x1172, 0xD29C), (0x1110, 0x1173, 0xD2B8),
    (0x1110, 0x1174, 0xD2D4), (0x1110, 0x1175, 0xD2F0), (0x1111, 0x1161, 0xD30C),
    (0x1111, 0x1162, 0xD328), (0x1111, 0x1163, 0xD344), (0x1111, 0x1164, 0xD360),
    (0x1111, 0x1165, 0xD37C), (0x1111, 0x1166, 0xD398), (0x1111, 0x1167, 0xD3B4),
    (0x1111, 0x1168, 0xD3D0), (0x1111, 0x1169, 0xD3EC), (0x1111, 0x116A, 0xD408),
    (0x1111, 0x116B, 0xD424), (0x1111, 0x116C, 0xD440), (0x1111, 0x116D, 0xD45C),
    (0x1111, 0x116E, 0xD478), (0x1111, 0x116F, 0xD494), (0x1111, 0x1170, 0xD4B0),
    (0x1111, 0x1171, 0xD4CC), (0x1111, 0x1172, 0xD4E8), (0x1111, 0x1173, 0xD504),
    (0x1111, 0x1174, 0xD520), (0x1111, 0x1175, 0xD53C), (0x1112, 0x1161, 0xD558),
    (0x1112, 0x1162, 0xD574), (0x1112, 0x1163, 0xD590), (0x1112, 0x1164, 0xD5AC),
    (0x1112, 0x1165, 0xD5C8), (0x1112, 0x1166, 0xD5E4), (0x1112, 0x1167, 0xD600),
    (0x1112, 0x1168, 0xD61C), (0x1112, 0x1169, 0xD638), (0x1112, 0x116A, 0xD654),
    (0x1112, 0x116B, 0xD670), (0x1112, 0x116C, 0xD68C), (0x1112, 0x116D, 0xD6A8),
    (0x1112, 0x116E, 0xD6C4), (0x1112, 0x116F, 0xD6E0), (0x1112, 0x1170, 0xD6FC),
    (0x1112, 0x1171, 0xD718), (0x1112, 0x1172, 0xD734), (0x1112, 0x1173, 0xD750),
    (0x1112, 0x1174, 0xD76C), (0x1112, 0x1175, 0xD788), (0x1B05, 0x1B35, 0x1B06),
    (0x1B07, 0x1B35, 0x1B08), (0x1B09, 0x1B35, 0x1B0A), (0x1B0B, 0x1B35, 0x1B0C),
    (0x1B0D, 0x1B35, 0x1B0E), (0x1B11, 0x1B35, 0x1B12), (0x1B3A, 0x1B35, 0x1B3B),
    (0x1B3C, 0x1B35, 0x1B3D), (0x1B3E, 0x1B35, 0x1B40), (0x1B3F, 0x1B35, 0x1B41),
    (0x1B42, 0x1B35, 0x1B43), (0x1FBF, 0x0300, 0x1FCD), (0x1FBF, 0x0301, 0x1FCE),
    (0x1FBF, 0x0342, 0x1FCF), (0x1FFE, 0x0300, 0x1FDD), (0x1FFE, 0x0301, 0x1FDE),
    (0x1FFE, 0x0342, 0x1FDF), (0x2190, 0x0338, 0x219A), (0x2192, 0x0338, 0x219B),
    (0x2194, 0x0338, 0x21AE), (0x21D0, 0x0338, 0x21CD), (0x21D2, 0x0338, 0x21CF),
    (0x21D4, 0x0338, 0x21CE), (0x2203, 0x0338, 0x2204), (0x2208, 0x0338, 0x2209),
    (0x220B, 0x0338, 0x220C), (0x2223, 0x0338, 0x2224), (0x2225, 0x0338, 0x2226),
    (0x223C, 0x0338, 0x2241), (0x2243, 0x0338, 0x2244), (0x2245, 0x0338, 0x2247),
    (0x2248, 0x0338, 0x2249), (0x224D, 0x0338, 0x226D), (0x2261, 0x0338, 0x2262),
    (0x2264, 0x0338, 0x2270), (0x2265, 0x0338, 0x2271), (0x2272, 0x0338, 0x2274),
    (0x2273, 0x0338, 0x2275), (0x2276, 0x0338, 0x2278), (0x2277, 0x0338, 0x2279),
    (0x227A, 0x0338, 0x2280), (0x227B, 0x0338, 0x2281), (0x227C, 0x0338, 0x22E0),
    (0x227D, 0x0338, 0x22E1), (0x2282, 0x0338, 0x2284), (0x2283, 0x0338, 0x2285),
    (0x2286, 0x0338, 0x2288), (0x2287, 0x0338, 0x2289), (0x2291, 0x0338, 0x22E2),
    (0x2292, 0x0338, 0x22E3), (0x22A2, 0x0338, 0x22AC), (0x22A8, 0x0338, 0x22AD),
    (0x22A9, 0x0338, 0x22AE), (0x22AB, 0x0338, 0x22AF), (0x22B2, 0x0338, 0x22EA),
    (0x22B3, 0x0338, 0x22EB), (0x22B4, 0x0338, 0x22EC), (0x22B5, 0x0338, 0x22ED),
    (0x3046, 0x3099, 0x3094), (0x304B, 0x3099, 0x304C), (0x304D, 0x3099, 0x304E),
    (0x304F, 0x3099, 0x3050), (0x3051, 0x3099, 0x3052), (0x3053, 0x3099, 0x3054),
    (0x3055, 0x3099, 0x3056), (0x3057, 0x3099, 0x3058), (0x3059, 0x3099, 0x305A),
    (0x305B, 0x3099, 0x305C), (0x305D, 0x3099, 0x305E), (0x305F, 0x3099, 0x3060),
    (0x3061, 0x3099, 0x3062), (0x3064, 0x3099, 0x3065), (0x3066, 0x3099, 0x3067),
    (0x3068, 0x3099, 0x3069), (0x306F, 0x3099, 0x3070), (0x306F, 0x309A, 0x3071),
    (0x3072, 0x3099, 0x3073), (0x3072, 0x309A, 0x3074), (0x3075, 0x3099, 0x3076),
    (0x3075, 0x309A, 0x3077), (0x3078, 0x3099, 0x3079), (0x3078, 0x309A, 0x307A),
    (0x307B, 0x3099, 0x307C), (0x307B, 0x309A, 0x307D), (0x309D, 0x3099, 0x309E),
    (0x30A6, 0x3099, 0x30F4), (0x30AB, 0x3099, 0x30AC), (0x30AD, 0x3099, 0x30AE),
    (0x30AF, 0x3099, 0x30B0), (0x30B1, 0x3099, 0x30B2), (0x30B3, 0x3099, 0x30B4),
    (0x30B5, 0x3099, 0x30B6), (0x30B7, 0x3099, 0x30B8), (0x30B9, 0x3099, 0x30BA),
    (0x30BB, 0x3099, 0x30BC), (0x30BD, 0x3099, 0x30BE), (0x30BF, 0x3099, 0x30C0),
    (0x30C1, 0x3099, 0x30C2), (0x30C4, 0x3099, 0x30C5), (0x30C6, 0x3099, 0x30C7),
    (0x30C8, 0x3099, 0x30C9), (0x30CF, 0x3099, 0x30D0), (0x30CF, 0x309A, 0x30D1),
    (0x30D2, 0x3099, 0x30D3), (0x30D2, 0x309A, 0x30D4), (0x30D5, 0x3099, 0x30D6),
    (0x30D5, 0x309A, 0x30D7), (0x30D8, 0x3099, 0x30D9), (0x30D8, 0x309A, 0x30DA),
    (0x30DB, 0x3099, 0x30DC), (0x30DB, 0x309A, 0x30DD), (0x30EF, 0x3099, 0x30F7),
    (0x30F0, 0x3099, 0x30F8), (0x30F1, 0x3099, 0x30F9), (0x30F2, 0x3099, 0x30FA),
    (0x30FD, 0x3099, 0x30FE), (0x105D2, 0x0307, 0x105C9), (0x105DA, 0x0307, 0x105E4),
    (0x11099, 0x110BA, 0x1109A), (0x1109B, 0x110BA, 0x1109C), (0x110A5, 0x110BA, 0x110AB),
    (0x11131, 0x11127, 0x1112E), (0x11132, 0x11127, 0x1112F), (0x11347, 0x1133E, 0x1134B),
    (0x11347, 0x11357, 0x1134C), (0x11382, 0x113C9, 0x11383), (0x11384, 0x113BB, 0x11385),
    (0x1138B, 0x113C2, 0x1138E), (0x11390, 0x113C9, 0x11391), (0x113C2, 0x113B8, 0x113C7),
    (0x113C2, 0x113C2, 0x113C5), (0x113C2, 0x113C9, 0x113C8), (0x114B9, 0x114B0, 0x114BC),
    (0x114B9, 0x114BA, 0x114BB), (0x114B9, 0x114BD, 0x114BE), (0x115B8, 0x115AF, 0x115BA),
    (0x115B9, 0x115AF, 0x115BB), (0x11935, 0x11930, 0x11938), (0x1611E, 0x1611E, 0x16121),
    (0x1611E, 0x1611F, 0x16123), (0x1611E, 0x16120, 0x16125), (0x1611E, 0x16129, 0x16122),
    (0x16129, 0x1611F, 0x16124), (0x16D63, 0x16D67, 0x16D69), (0x16D67, 0x16D67, 0x16D68),
];

/// The compatibility decomposition of every character NFKD changes, fully expanded and in code point
/// order. Hangul syllables are absent: [`decompose`] expands them arithmetically.
#[rustfmt::skip]
const NFKD_TABLE: &[(u32, &str)] = &[
    (0x00A0, " "), (0x00A8, " \u{308}"), (0x00AA, "a"), (0x00AF, " \u{304}"),
    (0x00B2, "2"), (0x00B3, "3"), (0x00B4, " \u{301}"), (0x00B5, "\u{3bc}"),
    (0x00B8, " \u{327}"), (0x00B9, "1"), (0x00BA, "o"), (0x00BC, "1\u{2044}4"),
    (0x00BD, "1\u{2044}2"), (0x00BE, "3\u{2044}4"), (0x00C0, "A\u{300}"), (0x00C1, "A\u{301}"),
    (0x00C2, "A\u{302}"), (0x00C3, "A\u{303}"), (0x00C4, "A\u{308}"), (0x00C5, "A\u{30a}"),
    (0x00C7, "C\u{327}"), (0x00C8, "E\u{300}"), (0x00C9, "E\u{301}"), (0x00CA, "E\u{302}"),
    (0x00CB, "E\u{308}"), (0x00CC, "I\u{300}"), (0x00CD, "I\u{301}"), (0x00CE, "I\u{302}"),
    (0x00CF, "I\u{308}"), (0x00D1, "N\u{303}"), (0x00D2, "O\u{300}"), (0x00D3, "O\u{301}"),
    (0x00D4, "O\u{302}"), (0x00D5, "O\u{303}"), (0x00D6, "O\u{308}"), (0x00D9, "U\u{300}"),
    (0x00DA, "U\u{301}"), (0x00DB, "U\u{302}"), (0x00DC, "U\u{308}"), (0x00DD, "Y\u{301}"),
    (0x00E0, "a\u{300}"), (0x00E1, "a\u{301}"), (0x00E2, "a\u{302}"), (0x00E3, "a\u{303}"),
    (0x00E4, "a\u{308}"), (0x00E5, "a\u{30a}"), (0x00E7, "c\u{327}"), (0x00E8, "e\u{300}"),
    (0x00E9, "e\u{301}"), (0x00EA, "e\u{302}"), (0x00EB, "e\u{308}"), (0x00EC, "i\u{300}"),
    (0x00ED, "i\u{301}"), (0x00EE, "i\u{302}"), (0x00EF, "i\u{308}"), (0x00F1, "n\u{303}"),
    (0x00F2, "o\u{300}"), (0x00F3, "o\u{301}"), (0x00F4, "o\u{302}"), (0x00F5, "o\u{303}"),
    (0x00F6, "o\u{308}"), (0x00F9, "u\u{300}"), (0x00FA, "u\u{301}"), (0x00FB, "u\u{302}"),
    (0x00FC, "u\u{308}"), (0x00FD, "y\u{301}"), (0x00FF, "y\u{308}"), (0x0100, "A\u{304}"),
    (0x0101, "a\u{304}"), (0x0102, "A\u{306}"), (0x0103, "a\u{306}"), (0x0104, "A\u{328}"),
    (0x0105, "a\u{328}"), (0x0106, "C\u{301}"), (0x0107, "c\u{301}"), (0x0108, "C\u{302}"),
    (0x0109, "c\u{302}"), (0x010A, "C\u{307}"), (0x010B, "c\u{307}"), (0x010C, "C\u{30c}"),
    (0x010D, "c\u{30c}"), (0x010E, "D\u{30c}"), (0x010F, "d\u{30c}"), (0x0112, "E\u{304}"),
    (0x0113, "e\u{304}"), (0x0114, "E\u{306}"), (0x0115, "e\u{306}"), (0x0116, "E\u{307}"),
    (0x0117, "e\u{307}"), (0x0118, "E\u{328}"), (0x0119, "e\u{328}"), (0x011A, "E\u{30c}"),
    (0x011B, "e\u{30c}"), (0x011C, "G\u{302}"), (0x011D, "g\u{302}"), (0x011E, "G\u{306}"),
    (0x011F, "g\u{306}"), (0x0120, "G\u{307}"), (0x0121, "g\u{307}"), (0x0122, "G\u{327}"),
    (0x0123, "g\u{327}"), (0x0124, "H\u{302}"), (0x0125, "h\u{302}"), (0x0128, "I\u{303}"),
    (0x0129, "i\u{303}"), (0x012A, "I\u{304}"), (0x012B, "i\u{304}"), (0x012C, "I\u{306}"),
    (0x012D, "i\u{306}"), (0x012E, "I\u{328}"), (0x012F, "i\u{328}"), (0x0130, "I\u{307}"),
    (0x0132, "IJ"), (0x0133, "ij"), (0x0134, "J\u{302}"), (0x0135, "j\u{302}"),
    (0x0136, "K\u{327}"), (0x0137, "k\u{327}"), (0x0139, "L\u{301}"), (0x013A, "l\u{301}"),
    (0x013B, "L\u{327}"), (0x013C, "l\u{327}"), (0x013D, "L\u{30c}"), (0x013E, "l\u{30c}"),
    (0x013F, "L\u{b7}"), (0x0140, "l\u{b7}"), (0x0143, "N\u{301}"), (0x0144, "n\u{301}"),
    (0x0145, "N\u{327}"), (0x0146, "n\u{327}"), (0x0147, "N\u{30c}"), (0x0148, "n\u{30c}"),
    (0x0149, "\u{2bc}n"), (0x014C, "O\u{304}"), (0x014D, "o\u{304}"), (0x014E, "O\u{306}"),
    (0x014F, "o\u{306}"), (0x0150, "O\u{30b}"), (0x0151, "o\u{30b}"), (0x0154, "R\u{301}"),
    (0x0155, "r\u{301}"), (0x0156, "R\u{327}"), (0x0157, "r\u{327}"), (0x0158, "R\u{30c}"),
    (0x0159, "r\u{30c}"), (0x015A, "S\u{301}"), (0x015B, "s\u{301}"), (0x015C, "S\u{302}"),
    (0x015D, "s\u{302}"), (0x015E, "S\u{327}"), (0x015F, "s\u{327}"), (0x0160, "S\u{30c}"),
    (0x0161, "s\u{30c}"), (0x0162, "T\u{327}"), (0x0163, "t\u{327}"), (0x0164, "T\u{30c}"),
    (0x0165, "t\u{30c}"), (0x0168, "U\u{303}"), (0x0169, "u\u{303}"), (0x016A, "U\u{304}"),
    (0x016B, "u\u{304}"), (0x016C, "U\u{306}"), (0x016D, "u\u{306}"), (0x016E, "U\u{30a}"),
    (0x016F, "u\u{30a}"), (0x0170, "U\u{30b}"), (0x0171, "u\u{30b}"), (0x0172, "U\u{328}"),
    (0x0173, "u\u{328}"), (0x0174, "W\u{302}"), (0x0175, "w\u{302}"), (0x0176, "Y\u{302}"),
    (0x0177, "y\u{302}"), (0x0178, "Y\u{308}"), (0x0179, "Z\u{301}"), (0x017A, "z\u{301}"),
    (0x017B, "Z\u{307}"), (0x017C, "z\u{307}"), (0x017D, "Z\u{30c}"), (0x017E, "z\u{30c}"),
    (0x017F, "s"), (0x01A0, "O\u{31b}"), (0x01A1, "o\u{31b}"), (0x01AF, "U\u{31b}"),
    (0x01B0, "u\u{31b}"), (0x01C4, "DZ\u{30c}"), (0x01C5, "Dz\u{30c}"), (0x01C6, "dz\u{30c}"),
    (0x01C7, "LJ"), (0x01C8, "Lj"), (0x01C9, "lj"), (0x01CA, "NJ"),
    (0x01CB, "Nj"), (0x01CC, "nj"), (0x01CD, "A\u{30c}"), (0x01CE, "a\u{30c}"),
    (0x01CF, "I\u{30c}"), (0x01D0, "i\u{30c}"), (0x01D1, "O\u{30c}"), (0x01D2, "o\u{30c}"),
    (0x01D3, "U\u{30c}"), (0x01D4, "u\u{30c}"), (0x01D5, "U\u{308}\u{304}"), (0x01D6, "u\u{308}\u{304}"),
    (0x01D7, "U\u{308}\u{301}"), (0x01D8, "u\u{308}\u{301}"), (0x01D9, "U\u{308}\u{30c}"), (0x01DA, "u\u{308}\u{30c}"),
    (0x01DB, "U\u{308}\u{300}"), (0x01DC, "u\u{308}\u{300}"), (0x01DE, "A\u{308}\u{304}"), (0x01DF, "a\u{308}\u{304}"),
    (0x01E0, "A\u{307}\u{304}"), (0x01E1, "a\u{307}\u{304}"), (0x01E2, "\u{c6}\u{304}"), (0x01E3, "\u{e6}\u{304}"),
    (0x01E6, "G\u{30c}"), (0x01E7, "g\u{30c}"), (0x01E8, "K\u{30c}"), (0x01E9, "k\u{30c}"),
    (0x01EA, "O\u{328}"), (0x01EB, "o\u{328}"), (0x01EC, "O\u{328}\u{304}"), (0x01ED, "o\u{328}\u{304}"),
    (0x01EE, "\u{1b7}\u{30c}"), (0x01EF, "\u{292}\u{30c}"), (0x01F0, "j\u{30c}"), (0x01F1, "DZ"),
    (0x01F2, "Dz"), (0x01F3, "dz"), (0x01F4, "G\u{301}"), (0x01F5, "g\u{301}"),
    (0x01F8, "N\u{300}"), (0x01F9, "n\u{300}"), (0x01FA, "A\u{30a}\u{301}"), (0x01FB, "a\u{30a}\u{301}"),
    (0x01FC, "\u{c6}\u{301}"), (0x01FD, "\u{e6}\u{301}"), (0x01FE, "\u{d8}\u{301}"), (0x01FF, "\u{f8}\u{301}"),
    (0x0200, "A\u{30f}"), (0x0201, "a\u{30f}"), (0x0202, "A\u{311}"), (0x0203, "a\u{311}"),
    (0x0204, "E\u{30f}"), (0x0205, "e\u{30f}"), (0x0206, "E\u{311}"), (0x0207, "e\u{311}"),
    (0x0208, "I\u{30f}"), (0x0209, "i\u{30f}"), (0x020A, "I\u{311}"), (0x020B, "i\u{311}"),
    (0x020C, "O\u{30f}"), (0x020D, "o\u{30f}"), (0x020E, "O\u{311}"), (0x020F, "o\u{311}"),
    (0x0210, "R\u{30f}"), (0x0211, "r\u{30f}"), (0x0212, "R\u{311}"), (0x0213, "r\u{311}"),
    (0x0214, "U\u{30f}"), (0x0215, "u\u{30f}"), (0x0216, "U\u{311}"), (0x0217, "u\u{311}"),
    (0x0218, "S\u{326}"), (0x0219, "s\u{326}"), (0x021A, "T\u{326}"), (0x021B, "t\u{326}"),
    (0x021E, "H\u{30c}"), (0x021F, "h\u{30c}"), (0x0226, "A\u{307}"), (0x0227, "a\u{307}"),
    (0x0228, "E\u{327}"), (0x0229, "e\u{327}"), (0x022A, "O\u{308}\u{304}"), (0x022B, "o\u{308}\u{304}"),
    (0x022C, "O\u{303}\u{304}"), (0x022D, "o\u{303}\u{304}"), (0x022E, "O\u{307}"), (0x022F, "o\u{307}"),
    (0x0230, "O\u{307}\u{304}"), (0x0231, "o\u{307}\u{304}"), (0x0232, "Y\u{304}"), (0x0233, "y\u{304}"),
    (0x02B0, "h"), (0x02B1, "\u{266}"), (0x02B2, "j"), (0x02B3, "r"),
    (0x02B4, "\u{279}"), (0x02B5, "\u{27b}"), (0x02B6, "\u{281}"), (0x02B7, "w"),
    (0x02B8, "y"), (0x02D8, " \u{306}"), (0x02D9, " \u{307}"), (0x02DA, " \u{30a}"),
    (0x02DB, " \u{328}"), (0x02DC, " \u{303}"), (0x02DD, " \u{30b}"), (0x02E0, "\u{263}"),
    (0x02E1, "l"), (0x02E2, "s"), (0x02E3, "x"), (0x02E4, "\u{295}"),
    (0x0340, "\u{300}"), (0x0341, "\u{301}"), (0x0343, "\u{313}"), (0x0344, "\u{308}\u{301}"),
    (0x0374, "\u{2b9}"), (0x037A, " \u{345}"), (0x037E, ";"), (0x0384, " \u{301}"),
    (0x0385, " \u{308}\u{301}"), (0x0386, "\u{391}\u{301}"), (0x0387, "\u{b7}"), (0x0388, "\u{395}\u{301}"),
    (0x0389, "\u{397}\u{301}"), (0x038A, "\u{399}\u{301}"), (0x038C, "\u{39f}\u{301}"), (0x038E, "\u{3a5}\u{301}"),
    (0x038F, "\u{3a9}\u{301}"), (0x0390, "\u{3b9}\u{308}\u{301}"), (0x03AA, "\u{399}\u{308}"), (0x03AB, "\u{3a5}\u{308}"),
    (0x03AC, "\u{3b1}\u{301}"), (0x03AD, "\u{3b5}\u{301}"), (0x03AE, "\u{3b7}\u{301}"), (0x03AF, "\u{3b9}\u{301}"),
    (0x03B0, "\u{3c5}\u{308}\u{301}"), (0x03CA, "\u{3b9}\u{308}"), (0x03CB, "\u{3c5}\u{308}"), (0x03CC, "\u{3bf}\u{301}"),
    (0x03CD, "\u{3c5}\u{301}"), (0x03CE, "\u{3c9}\u{301}"), (0x03D0, "\u{3b2}"), (0x03D1, "\u{3b8}"),
    (0x03D2, "\u{3a5}"), (0x03D3, "\u{3a5}\u{301}"), (0x03D4, "\u{3a5}\u{308}"), (0x03D5, "\u{3c6}"),
    (0x03D6, "\u{3c0}"), (0x03F0, "\u{3ba}"), (0x03F1, "\u{3c1}"), (0x03F2, "\u{3c2}"),
    (0x03F4, "\u{398}"), (0x03F5, "\u{3b5}"), (0x03F9, "\u{3a3}"), (0x0400, "\u{415}\u{300}"),
    (0x0401, "\u{415}\u{308}"), (0x0403, "\u{413}\u{301}"), (0x0407, "\u{406}\u{308}"), (0x040C, "\u{41a}\u{301}"),
    (0x040D, "\u{418}\u{300}"), (0x040E, "\u{423}\u{306}"), (0x0419, "\u{418}\u{306}"), (0x0439, "\u{438}\u{306}"),
    (0x0450, "\u{435}\u{300}"), (0x0451, "\u{435}\u{308}"), (0x0453, "\u{433}\u{301}"), (0x0457, "\u{456}\u{308}"),
    (0x045C, "\u{43a}\u{301}"), (0x045D, "\u{438}\u{300}"), (0x045E, "\u{443}\u{306}"), (0x0476, "\u{474}\u{30f}"),
    (0x0477, "\u{475}\u{30f}"), (0x04C1, "\u{416}\u{306}"), (0x04C2, "\u{436}\u{306}"), (0x04D0, "\u{410}\u{306}"),
    (0x04D1, "\u{430}\u{306}"), (0x04D2, "\u{410}\u{308}"), (0x04D3, "\u{430}\u{308}"), (0x04D6, "\u{415}\u{306}"),
    (0x04D7, "\u{435}\u{306}"), (0x04DA, "\u{4d8}\u{308}"), (0x04DB, "\u{4d9}\u{308}"), (0x04DC, "\u{416}\u{308}"),
    (0x04DD, "\u{436}\u{308}"), (0x04DE, "\u{417}\u{308}"), (0x04DF, "\u{437}\u{308}"), (0x04E2, "\u{418}\u{304}"),
    (0x04E3, "\u{438}\u{304}"), (0x04E4, "\u{418}\u{308}"), (0x04E5, "\u{438}\u{308}"), (0x04E6, "\u{41e}\u{308}"),
    (0x04E7, "\u{43e}\u{308}"), (0x04EA, "\u{4e8}\u{308}"), (0x04EB, "\u{4e9}\u{308}"), (0x04EC, "\u{42d}\u{308}"),
    (0x04ED, "\u{44d}\u{308}"), (0x04EE, "\u{423}\u{304}"), (0x04EF, "\u{443}\u{304}"), (0x04F0, "\u{423}\u{308}"),
    (0x04F1, "\u{443}\u{308}"), (0x04F2, "\u{423}\u{30b}"), (0x04F3, "\u{443}\u{30b}"), (0x04F4, "\u{427}\u{308}"),
    (0x04F5, "\u{447}\u{308}"), (0x04F8, "\u{42b}\u{308}"), (0x04F9, "\u{44b}\u{308}"), (0x0587, "\u{565}\u{582}"),
    (0x0622, "\u{627}\u{653}"), (0x0623, "\u{627}\u{654}"), (0x0624, "\u{648}\u{654}"), (0x0625, "\u{627}\u{655}"),
    (0x0626, "\u{64a}\u{654}"), (0x0675, "\u{627}\u{674}"), (0x0676, "\u{648}\u{674}"), (0x0677, "\u{6c7}\u{674}"),
    (0x0678, "\u{64a}\u{674}"), (0x06C0, "\u{6d5}\u{654}"), (0x06C2, "\u{6c1}\u{654}"), (0x06D3, "\u{6d2}\u{654}"),
    (0x0929, "\u{928}\u{93c}"), (0x0931, "\u{930}\u{93c}"), (0x0934, "\u{933}\u{93c}"), (0x0958, "\u{915}\u{93c}"),
    (0x0959, "\u{916}\u{93c}"), (0x095A, "\u{917}\u{93c}"), (0x095B, "\u{91c}\u{93c}"), (0x095C, "\u{921}\u{93c}"),
    (0x095D, "\u{922}\u{93c}"), (0x095E, "\u{92b}\u{93c}"), (0x095F, "\u{92f}\u{93c}"), (0x09CB, "\u{9c7}\u{9be}"),
    (0x09CC, "\u{9c7}\u{9d7}"), (0x09DC, "\u{9a1}\u{9bc}"), (0x09DD, "\u{9a2}\u{9bc}"), (0x09DF, "\u{9af}\u{9bc}"),
    (0x0A33, "\u{a32}\u{a3c}"), (0x0A36, "\u{a38}\u{a3c}"), (0x0A59, "\u{a16}\u{a3c}"), (0x0A5A, "\u{a17}\u{a3c}"),
    (0x0A5B, "\u{a1c}\u{a3c}"), (0x0A5E, "\u{a2b}\u{a3c}"), (0x0B48, "\u{b47}\u{b56}"), (0x0B4B, "\u{b47}\u{b3e}"),
    (0x0B4C, "\u{b47}\u{b57}"), (0x0B5C, "\u{b21}\u{b3c}"), (0x0B5D, "\u{b22}\u{b3c}"), (0x0B94, "\u{b92}\u{bd7}"),
    (0x0BCA, "\u{bc6}\u{bbe}"), (0x0BCB, "\u{bc7}\u{bbe}"), (0x0BCC, "\u{bc6}\u{bd7}"), (0x0C48, "\u{c46}\u{c56}"),
    (0x0CC0, "\u{cbf}\u{cd5}"), (0x0CC7, "\u{cc6}\u{cd5}"), (0x0CC8, "\u{cc6}\u{cd6}"), (0x0CCA, "\u{cc6}\u{cc2}"),
    (0x0CCB, "\u{cc6}\u{cc2}\u{cd5}"), (0x0D4A, "\u{d46}\u{d3e}"), (0x0D4B, "\u{d47}\u{d3e}"), (0x0D4C, "\u{d46}\u{d57}"),
    (0x0DDA, "\u{dd9}\u{dca}"), (0x0DDC, "\u{dd9}\u{dcf}"), (0x0DDD, "\u{dd9}\u{dcf}\u{dca}"), (0x0DDE, "\u{dd9}\u{ddf}"),
    (0x0E33, "\u{e4d}\u{e32}"), (0x0EB3, "\u{ecd}\u{eb2}"), (0x0EDC, "\u{eab}\u{e99}"), (0x0EDD, "\u{eab}\u{ea1}"),
    (0x0F0C, "\u{f0b}"), (0x0F43, "\u{f42}\u{fb7}"), (0x0F4D, "\u{f4c}\u{fb7}"), (0x0F52, "\u{f51}\u{fb7}"),
    (0x0F57, "\u{f56}\u{fb7}"), (0x0F5C, "\u{f5b}\u{fb7}"), (0x0F69, "\u{f40}\u{fb5}"), (0x0F73, "\u{f71}\u{f72}"),
    (0x0F75, "\u{f71}\u{f74}"), (0x0F76, "\u{fb2}\u{f80}"), (0x0F77, "\u{fb2}\u{f71}\u{f80}"), (0x0F78, "\u{fb3}\u{f80}"),
    (0x0F79, "\u{fb3}\u{f71}\u{f80}"), (0x0F81, "\u{f71}\u{f80}"), (0x0F93, "\u{f92}\u{fb7}"), (0x0F9D, "\u{f9c}\u{fb7}"),
    (0x0FA2, "\u{fa1}\u{fb7}"), (0x0FA7, "\u{fa6}\u{fb7}"), (0x0FAC, "\u{fab}\u{fb7}"), (0x0FB9, "\u{f90}\u{fb5}"),
    (0x1026, "\u{1025}\u{102e}"), (0x10FC, "\u{10dc}"), (0x1B06, "\u{1b05}\u{1b35}"), (0x1B08, "\u{1b07}\u{1b35}"),
    (0x1B0A, "\u{1b09}\u{1b35}"), (0x1B0C, "\u{1b0b}\u{1b35}"), (0x1B0E, "\u{1b0d}\u{1b35}"), (0x1B12, "\u{1b11}\u{1b35}"),
    (0x1B3B, "\u{1b3a}\u{1b35}"), (0x1B3D, "\u{1b3c}\u{1b35}"), (0x1B40, "\u{1b3e}\u{1b35}"), (0x1B41, "\u{1b3f}\u{1b35}"),
    (0x1B43, "\u{1b42}\u{1b35}"), (0x1D2C, "A"), (0x1D2D, "\u{c6}"), (0x1D2E, "B"),
    (0x1D30, "D"), (0x1D31, "E"), (0x1D32, "\u{18e}"), (0x1D33, "G"),
    (0x1D34, "H"), (0x1D35, "I"), (0x1D36, "J"), (0x1D37, "K"),
    (0x1D38, "L"), (0x1D39, "M"), (0x1D3A, "N"), (0x1D3C, "O"),
    (0x1D3D, "\u{222}"), (0x1D3E, "P"), (0x1D3F, "R"), (0x1D40, "T"),
    (0x1D41, "U"), (0x1D42, "W"), (0x1D43, "a"), (0x1D44, "\u{250}"),
    (0x1D45, "\u{251}"), (0x1D46, "\u{1d02}"), (0x1D47, "b"), (0x1D48, "d"),
    (0x1D49, "e"), (0x1D4A, "\u{259}"), (0x1D4B, "\u{25b}"), (0x1D4C, "\u{25c}"),
    (0x1D4D, "g"), (0x1D4F, "k"), (0x1D50, "m"), (0x1D51, "\u{14b}"),
    (0x1D52, "o"), (0x1D53, "\u{254}"), (0x1D54, "\u{1d16}"), (0x1D55, "\u{1d17}"),
    (0x1D56, "p"), (0x1D57, "t"), (0x1D58, "u"), (0x1D59, "\u{1d1d}"),
    (0x1D5A, "\u{26f}"), (0x1D5B, "v"), (0x1D5C, "\u{1d25}"), (0x1D5D, "\u{3b2}"),
    (0x1D5E, "\u{3b3}"), (0x1D5F, "\u{3b4}"), (0x1D60, "\u{3c6}"), (0x1D61, "\u{3c7}"),
    (0x1D62, "i"), (0x1D63, "r"), (0x1D64, "u"), (0x1D65, "v"),
    (0x1D66, "\u{3b2}"), (0x1D67, "\u{3b3}"), (0x1D68, "\u{3c1}"), (0x1D69, "\u{3c6}"),
    (0x1D6A, "\u{3c7}"), (0x1D78, "\u{43d}"), (0x1D9B, "\u{252}"), (0x1D9C, "c"),
    (0x1D9D, "\u{255}"), (0x1D9E, "\u{f0}"), (0x1D9F, "\u{25c}"), (0x1DA0, "f"),
    (0x1DA1, "\u{25f}"), (0x1DA2, "\u{261}"), (0x1DA3, "\u{265}"), (0x1DA4, "\u{268}"),
    (0x1DA5, "\u{269}"), (0x1DA6, "\u{26a}"), (0x1DA7, "\u{1d7b}"), (0x1DA8, "\u{29d}"),
    (0x1DA9, "\u{26d}"), (0x1DAA, "\u{1d85}"), (0x1DAB, "\u{29f}"), (0x1DAC, "\u{271}"),
    (0x1DAD, "\u{270}"), (0x1DAE, "\u{272}"), (0x1DAF, "\u{273}"), (0x1DB0, "\u{274}"),
    (0x1DB1, "\u{275}"), (0x1DB2, "\u{278}"), (0x1DB3, "\u{282}"), (0x1DB4, "\u{283}"),
    (0x1DB5, "\u{1ab}"), (0x1DB6, "\u{289}"), (0x1DB7, "\u{28a}"), (0x1DB8, "\u{1d1c}"),
    (0x1DB9, "\u{28b}"), (0x1DBA, "\u{28c}"), (0x1DBB, "z"), (0x1DBC, "\u{290}"),
    (0x1DBD, "\u{291}"), (0x1DBE, "\u{292}"), (0x1DBF, "\u{3b8}"), (0x1E00, "A\u{325}"),
    (0x1E01, "a\u{325}"), (0x1E02, "B\u{307}"), (0x1E03, "b\u{307}"), (0x1E04, "B\u{323}"),
    (0x1E05, "b\u{323}"), (0x1E06, "B\u{331}"), (0x1E07, "b\u{331}"), (0x1E08, "C\u{327}\u{301}"),
    (0x1E09, "c\u{327}\u{301}"), (0x1E0A, "D\u{307}"), (0x1E0B, "d\u{307}"), (0x1E0C, "D\u{323}"),
    (0x1E0D, "d\u{323}"), (0x1E0E, "D\u{331}"), (0x1E0F, "d\u{331}"), (0x1E10, "D\u{327}"),
    (0x1E11, "d\u{327}"), (0x1E12, "D\u{32d}"), (0x1E13, "d\u{32d}"), (0x1E14, "E\u{304}\u{300}"),
    (0x1E15, "e\u{304}\u{300}"), (0x1E16, "E\u{304}\u{301}"), (0x1E17, "e\u{304}\u{301}"), (0x1E18, "E\u{32d}"),
    (0x1E19, "e\u{32d}"), (0x1E1A, "E\u{330}"), (0x1E1B, "e\u{330}"), (0x1E1C, "E\u{327}\u{306}"),
    (0x1E1D, "e\u{327}\u{306}"), (0x1E1E, "F\u{307}"), (0x1E1F, "f\u{307}"), (0x1E20, "G\u{304}"),
    (0x1E21, "g\u{304}"), (0x1E22, "H\u{307}"), (0x1E23, "h\u{307}"), (0x1E24, "H\u{323}"),
    (0x1E25, "h\u{323}"), (0x1E26, "H\u{308}"), (0x1E27, "h\u{308}"), (0x1E28, "H\u{327}"),
    (0x1E29, "h\u{327}"), (0x1E2A, "H\u{32e}"), (0x1E2B, "h\u{32e}"), (0x1E2C, "I\u{330}"),
    (0x1E2D, "i\u{330}"), (0x1E2E, "I\u{308}\u{301}"), (0x1E2F, "i\u{308}\u{301}"), (0x1E30, "K\u{301}"),
    (0x1E31, "k\u{301}"), (0x1E32, "K\u{323}"), (0x1E33, "k\u{323}"), (0x1E34, "K\u{331}"),
    (0x1E35, "k\u{331}"), (0x1E36, "L\u{323}"), (0x1E37, "l\u{323}"), (0x1E38, "L\u{323}\u{304}"),
    (0x1E39, "l\u{323}\u{304}"), (0x1E3A, "L\u{331}"), (0x1E3B, "l\u{331}"), (0x1E3C, "L\u{32d}"),
    (0x1E3D, "l\u{32d}"), (0x1E3E, "M\u{301}"), (0x1E3F, "m\u{301}"), (0x1E40, "M\u{307}"),
    (0x1E41, "m\u{307}"), (0x1E42, "M\u{323}"), (0x1E43, "m\u{323}"), (0x1E44, "N\u{307}"),
    (0x1E45, "n\u{307}"), (0x1E46, "N\u{323}"), (0x1E47, "n\u{323}"), (0x1E48, "N\u{331}"),
    (0x1E49, "n\u{331}"), (0x1E4A, "N\u{32d}"), (0x1E4B, "n\u{32d}"), (0x1E4C, "O\u{303}\u{301}"),
    (0x1E4D, "o\u{303}\u{301}"), (0x1E4E, "O\u{303}\u{308}"), (0x1E4F, "o\u{303}\u{308}"), (0x1E50, "O\u{304}\u{300}"),
    (0x1E51, "o\u{304}\u{300}"), (0x1E52, "O\u{304}\u{301}"), (0x1E53, "o\u{304}\u{301}"), (0x1E54, "P\u{301}"),
    (0x1E55, "p\u{301}"), (0x1E56, "P\u{307}"), (0x1E57, "p\u{307}"), (0x1E58, "R\u{307}"),
    (0x1E59, "r\u{307}"), (0x1E5A, "R\u{323}"), (0x1E5B, "r\u{323}"), (0x1E5C, "R\u{323}\u{304}"),
    (0x1E5D, "r\u{323}\u{304}"), (0x1E5E, "R\u{331}"), (0x1E5F, "r\u{331}"), (0x1E60, "S\u{307}"),
    (0x1E61, "s\u{307}"), (0x1E62, "S\u{323}"), (0x1E63, "s\u{323}"), (0x1E64, "S\u{301}\u{307}"),
    (0x1E65, "s\u{301}\u{307}"), (0x1E66, "S\u{30c}\u{307}"), (0x1E67, "s\u{30c}\u{307}"), (0x1E68, "S\u{323}\u{307}"),
    (0x1E69, "s\u{323}\u{307}"), (0x1E6A, "T\u{307}"), (0x1E6B, "t\u{307}"), (0x1E6C, "T\u{323}"),
    (0x1E6D, "t\u{323}"), (0x1E6E, "T\u{331}"), (0x1E6F, "t\u{331}"), (0x1E70, "T\u{32d}"),
    (0x1E71, "t\u{32d}"), (0x1E72, "U\u{324}"), (0x1E73, "u\u{324}"), (0x1E74, "U\u{330}"),
    (0x1E75, "u\u{330}"), (0x1E76, "U\u{32d}"), (0x1E77, "u\u{32d}"), (0x1E78, "U\u{303}\u{301}"),
    (0x1E79, "u\u{303}\u{301}"), (0x1E7A, "U\u{304}\u{308}"), (0x1E7B, "u\u{304}\u{308}"), (0x1E7C, "V\u{303}"),
    (0x1E7D, "v\u{303}"), (0x1E7E, "V\u{323}"), (0x1E7F, "v\u{323}"), (0x1E80, "W\u{300}"),
    (0x1E81, "w\u{300}"), (0x1E82, "W\u{301}"), (0x1E83, "w\u{301}"), (0x1E84, "W\u{308}"),
    (0x1E85, "w\u{308}"), (0x1E86, "W\u{307}"), (0x1E87, "w\u{307}"), (0x1E88, "W\u{323}"),
    (0x1E89, "w\u{323}"), (0x1E8A, "X\u{307}"), (0x1E8B, "x\u{307}"), (0x1E8C, "X\u{308}"),
    (0x1E8D, "x\u{308}"), (0x1E8E, "Y\u{307}"), (0x1E8F, "y\u{307}"), (0x1E90, "Z\u{302}"),
    (0x1E91, "z\u{302}"), (0x1E92, "Z\u{323}"), (0x1E93, "z\u{323}"), (0x1E94, "Z\u{331}"),
    (0x1E95, "z\u{331}"), (0x1E96, "h\u{331}"), (0x1E97, "t\u{308}"), (0x1E98, "w\u{30a}"),
    (0x1E99, "y\u{30a}"), (0x1E9A, "a\u{2be}"), (0x1E9B, "s\u{307}"), (0x1EA0, "A\u{323}"),
    (0x1EA1, "a\u{323}"), (0x1EA2, "A\u{309}"), (0x1EA3, "a\u{309}"), (0x1EA4, "A\u{302}\u{301}"),
    (0x1EA5, "a\u{302}\u{301}"), (0x1EA6, "A\u{302}\u{300}"), (0x1EA7, "a\u{302}\u{300}"), (0x1EA8, "A\u{302}\u{309}"),
    (0x1EA9, "a\u{302}\u{309}"), (0x1EAA, "A\u{302}\u{303}"), (0x1EAB, "a\u{302}\u{303}"), (0x1EAC, "A\u{323}\u{302}"),
    (0x1EAD, "a\u{323}\u{302}"), (0x1EAE, "A\u{306}\u{301}"), (0x1EAF, "a\u{306}\u{301}"), (0x1EB0, "A\u{306}\u{300}"),
    (0x1EB1, "a\u{306}\u{300}"), (0x1EB2, "A\u{306}\u{309}"), (0x1EB3, "a\u{306}\u{309}"), (0x1EB4, "A\u{306}\u{303}"),
    (0x1EB5, "a\u{306}\u{303}"), (0x1EB6, "A\u{323}\u{306}"), (0x1EB7, "a\u{323}\u{306}"), (0x1EB8, "E\u{323}"),
    (0x1EB9, "e\u{323}"), (0x1EBA, "E\u{309}"), (0x1EBB, "e\u{309}"), (0x1EBC, "E\u{303}"),
    (0x1EBD, "e\u{303}"), (0x1EBE, "E\u{302}\u{301}"), (0x1EBF, "e\u{302}\u{301}"), (0x1EC0, "E\u{302}\u{300}"),
    (0x1EC1, "e\u{302}\u{300}"), (0x1EC2, "E\u{302}\u{309}"), (0x1EC3, "e\u{302}\u{309}"), (0x1EC4, "E\u{302}\u{303}"),
    (0x1EC5, "e\u{302}\u{303}"), (0x1EC6, "E\u{323}\u{302}"), (0x1EC7, "e\u{323}\u{302}"), (0x1EC8, "I\u{309}"),
    (0x1EC9, "i\u{309}"), (0x1ECA, "I\u{323}"), (0x1ECB, "i\u{323}"), (0x1ECC, "O\u{323}"),
    (0x1ECD, "o\u{323}"), (0x1ECE, "O\u{309}"), (0x1ECF, "o\u{309}"), (0x1ED0, "O\u{302}\u{301}"),
    (0x1ED1, "o\u{302}\u{301}"), (0x1ED2, "O\u{302}\u{300}"), (0x1ED3, "o\u{302}\u{300}"), (0x1ED4, "O\u{302}\u{309}"),
    (0x1ED5, "o\u{302}\u{309}"), (0x1ED6, "O\u{302}\u{303}"), (0x1ED7, "o\u{302}\u{303}"), (0x1ED8, "O\u{323}\u{302}"),
    (0x1ED9, "o\u{323}\u{302}"), (0x1EDA, "O\u{31b}\u{301}"), (0x1EDB, "o\u{31b}\u{301}"), (0x1EDC, "O\u{31b}\u{300}"),
    (0x1EDD, "o\u{31b}\u{300}"), (0x1EDE, "O\u{31b}\u{309}"), (0x1EDF, "o\u{31b}\u{309}"), (0x1EE0, "O\u{31b}\u{303}"),
    (0x1EE1, "o\u{31b}\u{303}"), (0x1EE2, "O\u{31b}\u{323}"), (0x1EE3, "o\u{31b}\u{323}"), (0x1EE4, "U\u{323}"),
    (0x1EE5, "u\u{323}"), (0x1EE6, "U\u{309}"), (0x1EE7, "u\u{309}"), (0x1EE8, "U\u{31b}\u{301}"),
    (0x1EE9, "u\u{31b}\u{301}"), (0x1EEA, "U\u{31b}\u{300}"), (0x1EEB, "u\u{31b}\u{300}"), (0x1EEC, "U\u{31b}\u{309}"),
    (0x1EED, "u\u{31b}\u{309}"), (0x1EEE, "U\u{31b}\u{303}"), (0x1EEF, "u\u{31b}\u{303}"), (0x1EF0, "U\u{31b}\u{323}"),
    (0x1EF1, "u\u{31b}\u{323}"), (0x1EF2, "Y\u{300}"), (0x1EF3, "y\u{300}"), (0x1EF4, "Y\u{323}"),
    (0x1EF5, "y\u{323}"), (0x1EF6, "Y\u{309}"), (0x1EF7, "y\u{309}"), (0x1EF8, "Y\u{303}"),
    (0x1EF9, "y\u{303}"), (0x1F00, "\u{3b1}\u{313}"), (0x1F01, "\u{3b1}\u{314}"), (0x1F02, "\u{3b1}\u{313}\u{300}"),
    (0x1F03, "\u{3b1}\u{314}\u{300}"), (0x1F04, "\u{3b1}\u{313}\u{301}"), (0x1F05, "\u{3b1}\u{314}\u{301}"), (0x1F06, "\u{3b1}\u{313}\u{342}"),
    (0x1F07, "\u{3b1}\u{314}\u{342}"), (0x1F08, "\u{391}\u{313}"), (0x1F09, "\u{391}\u{314}"), (0x1F0A, "\u{391}\u{313}\u{300}"),
    (0x1F0B, "\u{391}\u{314}\u{300}"), (0x1F0C, "\u{391}\u{313}\u{301}"), (0x1F0D, "\u{391}\u{314}\u{301}"), (0x1F0E, "\u{391}\u{313}\u{342}"),
    (0x1F0F, "\u{391}\u{314}\u{342}"), (0x1F10, "\u{3b5}\u{313}"), (0x1F11, "\u{3b5}\u{314}"), (0x1F12, "\u{3b5}\u{313}\u{300}"),
    (0x1F13, "\u{3b5}\u{314}\u{300}"), (0x1F14, "\u{3b5}\u{313}\u{301}"), (0x1F15, "\u{3b5}\u{314}\u{301}"), (0x1F18, "\u{395}\u{313}"),
    (0x1F19, "\u{395}\u{314}"), (0x1F1A, "\u{395}\u{313}\u{300}"), (0x1F1B, "\u{395}\u{314}\u{300}"), (0x1F1C, "\u{395}\u{313}\u{301}"),
    (0x1F1D, "\u{395}\u{314}\u{301}"), (0x1F20, "\u{3b7}\u{313}"), (0x1F21, "\u{3b7}\u{314}"), (0x1F22, "\u{3b7}\u{313}\u{300}"),
    (0x1F23, "\u{3b7}\u{314}\u{300}"), (0x1F24, "\u{3b7}\u{313}\u{301}"), (0x1F25, "\u{3b7}\u{314}\u{301}"), (0x1F26, "\u{3b7}\u{313}\u{342}"),
    (0x1F27, "\u{3b7}\u{314}\u{342}"), (0x1F28, "\u{397}\u{313}"), (0x1F29, "\u{397}\u{314}"), (0x1F2A, "\u{397}\u{313}\u{300}"),
    (0x1F2B, "\u{397}\u{314}\u{300}"), (0x1F2C, "\u{397}\u{313}\u{301}"), (0x1F2D, "\u{397}\u{314}\u{301}"), (0x1F2E, "\u{397}\u{313}\u{342}"),
    (0x1F2F, "\u{397}\u{314}\u{342}"), (0x1F30, "\u{3b9}\u{313}"), (0x1F31, "\u{3b9}\u{314}"), (0x1F32, "\u{3b9}\u{313}\u{300}"),
    (0x1F33, "\u{3b9}\u{314}\u{300}"), (0x1F34, "\u{3b9}\u{313}\u{301}"), (0x1F35, "\u{3b9}\u{314}\u{301}"), (0x1F36, "\u{3b9}\u{313}\u{342}"),
    (0x1F37, "\u{3b9}\u{314}\u{342}"), (0x1F38, "\u{399}\u{313}"), (0x1F39, "\u{399}\u{314}"), (0x1F3A, "\u{399}\u{313}\u{300}"),
    (0x1F3B, "\u{399}\u{314}\u{300}"), (0x1F3C, "\u{399}\u{313}\u{301}"), (0x1F3D, "\u{399}\u{314}\u{301}"), (0x1F3E, "\u{399}\u{313}\u{342}"),
    (0x1F3F, "\u{399}\u{314}\u{342}"), (0x1F40, "\u{3bf}\u{313}"), (0x1F41, "\u{3bf}\u{314}"), (0x1F42, "\u{3bf}\u{313}\u{300}"),
    (0x1F43, "\u{3bf}\u{314}\u{300}"), (0x1F44, "\u{3bf}\u{313}\u{301}"), (0x1F45, "\u{3bf}\u{314}\u{301}"), (0x1F48, "\u{39f}\u{313}"),
    (0x1F49, "\u{39f}\u{314}"), (0x1F4A, "\u{39f}\u{313}\u{300}"), (0x1F4B, "\u{39f}\u{314}\u{300}"), (0x1F4C, "\u{39f}\u{313}\u{301}"),
    (0x1F4D, "\u{39f}\u{314}\u{301}"), (0x1F50, "\u{3c5}\u{313}"), (0x1F51, "\u{3c5}\u{314}"), (0x1F52, "\u{3c5}\u{313}\u{300}"),
    (0x1F53, "\u{3c5}\u{314}\u{300}"), (0x1F54, "\u{3c5}\u{313}\u{301}"), (0x1F55, "\u{3c5}\u{314}\u{301}"), (0x1F56, "\u{3c5}\u{313}\u{342}"),
    (0x1F57, "\u{3c5}\u{314}\u{342}"), (0x1F59, "\u{3a5}\u{314}"), (0x1F5B, "\u{3a5}\u{314}\u{300}"), (0x1F5D, "\u{3a5}\u{314}\u{301}"),
    (0x1F5F, "\u{3a5}\u{314}\u{342}"), (0x1F60, "\u{3c9}\u{313}"), (0x1F61, "\u{3c9}\u{314}"), (0x1F62, "\u{3c9}\u{313}\u{300}"),
    (0x1F63, "\u{3c9}\u{314}\u{300}"), (0x1F64, "\u{3c9}\u{313}\u{301}"), (0x1F65, "\u{3c9}\u{314}\u{301}"), (0x1F66, "\u{3c9}\u{313}\u{342}"),
    (0x1F67, "\u{3c9}\u{314}\u{342}"), (0x1F68, "\u{3a9}\u{313}"), (0x1F69, "\u{3a9}\u{314}"), (0x1F6A, "\u{3a9}\u{313}\u{300}"),
    (0x1F6B, "\u{3a9}\u{314}\u{300}"), (0x1F6C, "\u{3a9}\u{313}\u{301}"), (0x1F6D, "\u{3a9}\u{314}\u{301}"), (0x1F6E, "\u{3a9}\u{313}\u{342}"),
    (0x1F6F, "\u{3a9}\u{314}\u{342}"), (0x1F70, "\u{3b1}\u{300}"), (0x1F71, "\u{3b1}\u{301}"), (0x1F72, "\u{3b5}\u{300}"),
    (0x1F73, "\u{3b5}\u{301}"), (0x1F74, "\u{3b7}\u{300}"), (0x1F75, "\u{3b7}\u{301}"), (0x1F76, "\u{3b9}\u{300}"),
    (0x1F77, "\u{3b9}\u{301}"), (0x1F78, "\u{3bf}\u{300}"), (0x1F79, "\u{3bf}\u{301}"), (0x1F7A, "\u{3c5}\u{300}"),
    (0x1F7B, "\u{3c5}\u{301}"), (0x1F7C, "\u{3c9}\u{300}"), (0x1F7D, "\u{3c9}\u{301}"), (0x1F80, "\u{3b1}\u{313}\u{345}"),
    (0x1F81, "\u{3b1}\u{314}\u{345}"), (0x1F82, "\u{3b1}\u{313}\u{300}\u{345}"), (0x1F83, "\u{3b1}\u{314}\u{300}\u{345}"), (0x1F84, "\u{3b1}\u{313}\u{301}\u{345}"),
    (0x1F85, "\u{3b1}\u{314}\u{301}\u{345}"), (0x1F86, "\u{3b1}\u{313}\u{342}\u{345}"), (0x1F87, "\u{3b1}\u{314}\u{342}\u{345}"), (0x1F88, "\u{391}\u{313}\u{345}"),
    (0x1F89, "\u{391}\u{314}\u{345}"), (0x1F8A, "\u{391}\u{313}\u{300}\u{345}"), (0x1F8B, "\u{391}\u{314}\u{300}\u{345}"), (0x1F8C, "\u{391}\u{313}\u{301}\u{345}"),
    (0x1F8D, "\u{391}\u{314}\u{301}\u{345}"), (0x1F8E, "\u{391}\u{313}\u{342}\u{345}"), (0x1F8F, "\u{391}\u{314}\u{342}\u{345}"), (0x1F90, "\u{3b7}\u{313}\u{345}"),
    (0x1F91, "\u{3b7}\u{314}\u{345}"), (0x1F92, "\u{3b7}\u{313}\u{300}\u{345}"), (0x1F93, "\u{3b7}\u{314}\u{300}\u{345}"), (0x1F94, "\u{3b7}\u{313}\u{301}\u{345}"),
    (0x1F95, "\u{3b7}\u{314}\u{301}\u{345}"), (0x1F96, "\u{3b7}\u{313}\u{342}\u{345}"), (0x1F97, "\u{3b7}\u{314}\u{342}\u{345}"), (0x1F98, "\u{397}\u{313}\u{345}"),
    (0x1F99, "\u{397}\u{314}\u{345}"), (0x1F9A, "\u{397}\u{313}\u{300}\u{345}"), (0x1F9B, "\u{397}\u{314}\u{300}\u{345}"), (0x1F9C, "\u{397}\u{313}\u{301}\u{345}"),
    (0x1F9D, "\u{397}\u{314}\u{301}\u{345}"), (0x1F9E, "\u{397}\u{313}\u{342}\u{345}"), (0x1F9F, "\u{397}\u{314}\u{342}\u{345}"), (0x1FA0, "\u{3c9}\u{313}\u{345}"),
    (0x1FA1, "\u{3c9}\u{314}\u{345}"), (0x1FA2, "\u{3c9}\u{313}\u{300}\u{345}"), (0x1FA3, "\u{3c9}\u{314}\u{300}\u{345}"), (0x1FA4, "\u{3c9}\u{313}\u{301}\u{345}"),
    (0x1FA5, "\u{3c9}\u{314}\u{301}\u{345}"), (0x1FA6, "\u{3c9}\u{313}\u{342}\u{345}"), (0x1FA7, "\u{3c9}\u{314}\u{342}\u{345}"), (0x1FA8, "\u{3a9}\u{313}\u{345}"),
    (0x1FA9, "\u{3a9}\u{314}\u{345}"), (0x1FAA, "\u{3a9}\u{313}\u{300}\u{345}"), (0x1FAB, "\u{3a9}\u{314}\u{300}\u{345}"), (0x1FAC, "\u{3a9}\u{313}\u{301}\u{345}"),
    (0x1FAD, "\u{3a9}\u{314}\u{301}\u{345}"), (0x1FAE, "\u{3a9}\u{313}\u{342}\u{345}"), (0x1FAF, "\u{3a9}\u{314}\u{342}\u{345}"), (0x1FB0, "\u{3b1}\u{306}"),
    (0x1FB1, "\u{3b1}\u{304}"), (0x1FB2, "\u{3b1}\u{300}\u{345}"), (0x1FB3, "\u{3b1}\u{345}"), (0x1FB4, "\u{3b1}\u{301}\u{345}"),
    (0x1FB6, "\u{3b1}\u{342}"), (0x1FB7, "\u{3b1}\u{342}\u{345}"), (0x1FB8, "\u{391}\u{306}"), (0x1FB9, "\u{391}\u{304}"),
    (0x1FBA, "\u{391}\u{300}"), (0x1FBB, "\u{391}\u{301}"), (0x1FBC, "\u{391}\u{345}"), (0x1FBD, " \u{313}"),
    (0x1FBE, "\u{3b9}"), (0x1FBF, " \u{313}"), (0x1FC0, " \u{342}"), (0x1FC1, " \u{308}\u{342}"),
    (0x1FC2, "\u{3b7}\u{300}\u{345}"), (0x1FC3, "\u{3b7}\u{345}"), (0x1FC4, "\u{3b7}\u{301}\u{345}"), (0x1FC6, "\u{3b7}\u{342}"),
    (0x1FC7, "\u{3b7}\u{342}\u{345}"), (0x1FC8, "\u{395}\u{300}"), (0x1FC9, "\u{395}\u{301}"), (0x1FCA, "\u{397}\u{300}"),
    (0x1FCB, "\u{397}\u{301}"), (0x1FCC, "\u{397}\u{345}"), (0x1FCD, " \u{313}\u{300}"), (0x1FCE, " \u{313}\u{301}"),
    (0x1FCF, " \u{313}\u{342}"), (0x1FD0, "\u{3b9}\u{306}"), (0x1FD1, "\u{3b9}\u{304}"), (0x1FD2, "\u{3b9}\u{308}\u{300}"),
    (0x1FD3, "\u{3b9}\u{308}\u{301}"), (0x1FD6, "\u{3b9}\u{342}"), (0x1FD7, "\u{3b9}\u{308}\u{342}"), (0x1FD8, "\u{399}\u{306}"),
    (0x1FD9, "\u{399}\u{304}"), (0x1FDA, "\u{399}\u{300}"), (0x1FDB, "\u{399}\u{301}"), (0x1FDD, " \u{314}\u{300}"),
    (0x1FDE, " \u{314}\u{301}"), (0x1FDF, " \u{314}\u{342}"), (0x1FE0, "\u{3c5}\u{306}"), (0x1FE1, "\u{3c5}\u{304}"),
    (0x1FE2, "\u{3c5}\u{308}\u{300}"), (0x1FE3, "\u{3c5}\u{308}\u{301}"), (0x1FE4, "\u{3c1}\u{313}"), (0x1FE5, "\u{3c1}\u{314}"),
    (0x1FE6, "\u{3c5}\u{342}"), (0x1FE7, "\u{3c5}\u{308}\u{342}"), (0x1FE8, "\u{3a5}\u{306}"), (0x1FE9, "\u{3a5}\u{304}"),
    (0x1FEA, "\u{3a5}\u{300}"), (0x1FEB, "\u{3a5}\u{301}"), (0x1FEC, "\u{3a1}\u{314}"), (0x1FED, " \u{308}\u{300}"),
    (0x1FEE, " \u{308}\u{301}"), (0x1FEF, "`"), (0x1FF2, "\u{3c9}\u{300}\u{345}"), (0x1FF3, "\u{3c9}\u{345}"),
    (0x1FF4, "\u{3c9}\u{301}\u{345}"), (0x1FF6, "\u{3c9}\u{342}"), (0x1FF7, "\u{3c9}\u{342}\u{345}"), (0x1FF8, "\u{39f}\u{300}"),
    (0x1FF9, "\u{39f}\u{301}"), (0x1FFA, "\u{3a9}\u{300}"), (0x1FFB, "\u{3a9}\u{301}"), (0x1FFC, "\u{3a9}\u{345}"),
    (0x1FFD, " \u{301}"), (0x1FFE, " \u{314}"), (0x2000, " "), (0x2001, " "),
    (0x2002, " "), (0x2003, " "), (0x2004, " "), (0x2005, " "),
    (0x2006, " "), (0x2007, " "), (0x2008, " "), (0x2009, " "),
    (0x200A, " "), (0x2011, "\u{2010}"), (0x2017, " \u{333}"), (0x2024, "."),
    (0x2025, ".."), (0x2026, "..."), (0x202F, " "), (0x2033, "\u{2032}\u{2032}"),
    (0x2034, "\u{2032}\u{2032}\u{2032}"), (0x2036, "\u{2035}\u{2035}"), (0x2037, "\u{2035}\u{2035}\u{2035}"), (0x203C, "!!"),
    (0x203E, " \u{305}"), (0x2047, "??"), (0x2048, "?!"), (0x2049, "!?"),
    (0x2057, "\u{2032}\u{2032}\u{2032}\u{2032}"), (0x205F, " "), (0x2070, "0"), (0x2071, "i"),
    (0x2074, "4"), (0x2075, "5"), (0x2076, "6"), (0x2077, "7"),
    (0x2078, "8"), (0x2079, "9"), (0x207A, "+"), (0x207B, "\u{2212}"),
    (0x207C, "="), (0x207D, "("), (0x207E, ")"), (0x207F, "n"),
    (0x2080, "0"), (0x2081, "1"), (0x2082, "2"), (0x2083, "3"),
    (0x2084, "4"), (0x2085, "5"), (0x2086, "6"), (0x2087, "7"),
    (0x2088, "8"), (0x2089, "9"), (0x208A, "+"), (0x208B, "\u{2212}"),
    (0x208C, "="), (0x208D, "("), (0x208E, ")"), (0x2090, "a"),
    (0x2091, "e"), (0x2092, "o"), (0x2093, "x"), (0x2094, "\u{259}"),
    (0x2095, "h"), (0x2096, "k"), (0x2097, "l"), (0x2098, "m"),
    (0x2099, "n"), (0x209A, "p"), (0x209B, "s"), (0x209C, "t"),
    (0x20A8, "Rs"), (0x2100, "a/c"), (0x2101, "a/s"), (0x2102, "C"),
    (0x2103, "\u{b0}C"), (0x2105, "c/o"), (0x2106, "c/u"), (0x2107, "\u{190}"),
    (0x2109, "\u{b0}F"), (0x210A, "g"), (0x210B, "H"), (0x210C, "H"),
    (0x210D, "H"), (0x210E, "h"), (0x210F, "\u{127}"), (0x2110, "I"),
    (0x2111, "I"), (0x2112, "L"), (0x2113, "l"), (0x2115, "N"),
    (0x2116, "No"), (0x2119, "P"), (0x211A, "Q"), (0x211B, "R"),
    (0x211C, "R"), (0x211D, "R"), (0x2120, "SM"), (0x2121, "TEL"),
    (0x2122, "TM"), (0x2124, "Z"), (0x2126, "\u{3a9}"), (0x2128, "Z"),
    (0x212A, "K"), (0x212B, "A\u{30a}"), (0x212C, "B"), (0x212D, "C"),
    (0x212F, "e"), (0x2130, "E"), (0x2131, "F"), (0x2133, "M"),
    (0x2134, "o"), (0x2135, "\u{5d0}"), (0x2136, "\u{5d1}"), (0x2137, "\u{5d2}"),
    (0x2138, "\u{5d3}"), (0x2139, "i"), (0x213B, "FAX"), (0x213C, "\u{3c0}"),
    (0x213D, "\u{3b3}"), (0x213E, "\u{393}"), (0x213F, "\u{3a0}"), (0x2140, "\u{2211}"),
    (0x2145, "D"), (0x2146, "d"), (0x2147, "e"), (0x2148, "i"),
    (0x2149, "j"), (0x2150, "1\u{2044}7"), (0x2151, "1\u{2044}9"), (0x2152, "1\u{2044}10"),
    (0x2153, "1\u{2044}3"), (0x2154, "2\u{2044}3"), (0x2155, "1\u{2044}5"), (0x2156, "2\u{2044}5"),
    (0x2157, "3\u{2044}5"), (0x2158, "4\u{2044}5"), (0x2159, "1\u{2044}6"), (0x215A, "5\u{2044}6"),
    (0x215B, "1\u{2044}8"), (0x215C, "3\u{2044}8"), (0x215D, "5\u{2044}8"), (0x215E, "7\u{2044}8"),
    (0x215F, "1\u{2044}"), (0x2160, "I"), (0x2161, "II"), (0x2162, "III"),
    (0x2163, "IV"), (0x2164, "V"), (0x2165, "VI"), (0x2166, "VII"),
    (0x2167, "VIII"), (0x2168, "IX"), (0x2169, "X"), (0x216A, "XI"),
    (0x216B, "XII"), (0x216C, "L"), (0x216D, "C"), (0x216E, "D"),
    (0x216F, "M"), (0x2170, "i"), (0x2171, "ii"), (0x2172, "iii"),
    (0x2173, "iv"), (0x2174, "v"), (0x2175, "vi"), (0x2176, "vii"),
    (0x2177, "viii"), (0x2178, "ix"), (0x2179, "x"), (0x217A, "xi"),
    (0x217B, "xii"), (0x217C, "l"), (0x217D, "c"), (0x217E, "d"),
    (0x217F, "m"), (0x2189, "0\u{2044}3"), (0x219A, "\u{2190}\u{338}"), (0x219B, "\u{2192}\u{338}"),
    (0x21AE, "\u{2194}\u{338}"), (0x21CD, "\u{21d0}\u{338}"), (0x21CE, "\u{21d4}\u{338}"), (0x21CF, "\u{21d2}\u{338}"),
    (0x2204, "\u{2203}\u{338}"), (0x2209, "\u{2208}\u{338}"), (0x220C, "\u{220b}\u{338}"), (0x2224, "\u{2223}\u{338}"),
    (0x2226, "\u{2225}\u{338}"), (0x222C, "\u{222b}\u{222b}"), (0x222D, "\u{222b}\u{222b}\u{222b}"), (0x222F, "\u{222e}\u{222e}"),
    (0x2230, "\u{222e}\u{222e}\u{222e}"), (0x2241, "\u{223c}\u{338}"), (0x2244, "\u{2243}\u{338}"), (0x2247, "\u{2245}\u{338}"),
    (0x2249, "\u{2248}\u{338}"), (0x2260, "=\u{338}"), (0x2262, "\u{2261}\u{338}"), (0x226D, "\u{224d}\u{338}"),
    (0x226E, "<\u{338}"), (0x226F, ">\u{338}"), (0x2270, "\u{2264}\u{338}"), (0x2271, "\u{2265}\u{338}"),
    (0x2274, "\u{2272}\u{338}"), (0x2275, "\u{2273}\u{338}"), (0x2278, "\u{2276}\u{338}"), (0x2279, "\u{2277}\u{338}"),
    (0x2280, "\u{227a}\u{338}"), (0x2281, "\u{227b}\u{338}"), (0x2284, "\u{2282}\u{338}"), (0x2285, "\u{2283}\u{338}"),
    (0x2288, "\u{2286}\u{338}"), (0x2289, "\u{2287}\u{338}"), (0x22AC, "\u{22a2}\u{338}"), (0x22AD, "\u{22a8}\u{338}"),
    (0x22AE, "\u{22a9}\u{338}"), (0x22AF, "\u{22ab}\u{338}"), (0x22E0, "\u{227c}\u{338}"), (0x22E1, "\u{227d}\u{338}"),
    (0x22E2, "\u{2291}\u{338}"), (0x22E3, "\u{2292}\u{338}"), (0x22EA, "\u{22b2}\u{338}"), (0x22EB, "\u{22b3}\u{338}"),
    (0x22EC, "\u{22b4}\u{338}"), (0x22ED, "\u{22b5}\u{338}"), (0x2329, "\u{3008}"), (0x232A, "\u{3009}"),
    (0x2460, "1"), (0x2461, "2"), (0x2462, "3"), (0x2463, "4"),
    (0x2464, "5"), (0x2465, "6"), (0x2466, "7"), (0x2467, "8"),
    (0x2468, "9"), (0x2469, "10"), (0x246A, "11"), (0x246B, "12"),
    (0x246C, "13"), (0x246D, "14"), (0x246E, "15"), (0x246F, "16"),
    (0x2470, "17"), (0x2471, "18"), (0x2472, "19"), (0x2473, "20"),
    (0x2474, "(1)"), (0x2475, "(2)"), (0x2476, "(3)"), (0x2477, "(4)"),
    (0x2478, "(5)"), (0x2479, "(6)"), (0x247A, "(7)"), (0x247B, "(8)"),
    (0x247C, "(9)"), (0x247D, "(10)"), (0x247E, "(11)"), (0x247F, "(12)"),
    (0x2480, "(13)"), (0x2481, "(14)"), (0x2482, "(15)"), (0x2483, "(16)"),
    (0x2484, "(17)"), (0x2485, "(18)"), (0x2486, "(19)"), (0x2487, "(20)"),
    (0x2488, "1."), (0x2489, "2."), (0x248A, "3."), (0x248B, "4."),
    (0x248C, "5."), (0x248D, "6."), (0x248E, "7."), (0x248F, "8."),
    (0x2490, "9."), (0x2491, "10."), (0x2492, "11."), (0x2493, "12."),
    (0x2494, "13."), (0x2495, "14."), (0x2496, "15."), (0x2497, "16."),
    (0x2498, "17."), (0x2499, "18."), (0x249A, "19."), (0x249B, "20."),
    (0x249C, "(a)"), (0x249D, "(b)"), (0x249E, "(c)"), (0x249F, "(d)"),
    (0x24A0, "(e)"), (0x24A1, "(f)"), (0x24A2, "(g)"), (0x24A3, "(h)"),
    (0x24A4, "(i)"), (0x24A5, "(j)"), (0x24A6, "(k)"), (0x24A7, "(l)"),
    (0x24A8, "(m)"), (0x24A9, "(n)"), (0x24AA, "(o)"), (0x24AB, "(p)"),
    (0x24AC, "(q)"), (0x24AD, "(r)"), (0x24AE, "(s)"), (0x24AF, "(t)"),
    (0x24B0, "(u)"), (0x24B1, "(v)"), (0x24B2, "(w)"), (0x24B3, "(x)"),
    (0x24B4, "(y)"), (0x24B5, "(z)"), (0x24B6, "A"), (0x24B7, "B"),
    (0x24B8, "C"), (0x24B9, "D"), (0x24BA, "E"), (0x24BB, "F"),
    (0x24BC, "G"), (0x24BD, "H"), (0x24BE, "I"), (0x24BF, "J"),
    (0x24C0, "K"), (0x24C1, "L"), (0x24C2, "M"), (0x24C3, "N"),
    (0x24C4, "O"), (0x24C5, "P"), (0x24C6, "Q"), (0x24C7, "R"),
    (0x24C8, "S"), (0x24C9, "T"), (0x24CA, "U"), (0x24CB, "V"),
    (0x24CC, "W"), (0x24CD, "X"), (0x24CE, "Y"), (0x24CF, "Z"),
    (0x24D0, "a"), (0x24D1, "b"), (0x24D2, "c"), (0x24D3, "d"),
    (0x24D4, "e"), (0x24D5, "f"), (0x24D6, "g"), (0x24D7, "h"),
    (0x24D8, "i"), (0x24D9, "j"), (0x24DA, "k"), (0x24DB, "l"),
    (0x24DC, "m"), (0x24DD, "n"), (0x24DE, "o"), (0x24DF, "p"),
    (0x24E0, "q"), (0x24E1, "r"), (0x24E2, "s"), (0x24E3, "t"),
    (0x24E4, "u"), (0x24E5, "v"), (0x24E6, "w"), (0x24E7, "x"),
    (0x24E8, "y"), (0x24E9, "z"), (0x24EA, "0"), (0x2A0C, "\u{222b}\u{222b}\u{222b}\u{222b}"),
    (0x2A74, "::="), (0x2A75, "=="), (0x2A76, "==="), (0x2ADC, "\u{2add}\u{338}"),
    (0x2C7C, "j"), (0x2C7D, "V"), (0x2D6F, "\u{2d61}"), (0x2E9F, "\u{6bcd}"),
    (0x2EF3, "\u{9f9f}"), (0x2F00, "\u{4e00}"), (0x2F01, "\u{4e28}"), (0x2F02, "\u{4e36}"),
    (0x2F03, "\u{4e3f}"), (0x2F04, "\u{4e59}"), (0x2F05, "\u{4e85}"), (0x2F06, "\u{4e8c}"),
    (0x2F07, "\u{4ea0}"), (0x2F08, "\u{4eba}"), (0x2F09, "\u{513f}"), (0x2F0A, "\u{5165}"),
    (0x2F0B, "\u{516b}"), (0x2F0C, "\u{5182}"), (0x2F0D, "\u{5196}"), (0x2F0E, "\u{51ab}"),
    (0x2F0F, "\u{51e0}"), (0x2F10, "\u{51f5}"), (0x2F11, "\u{5200}"), (0x2F12, "\u{529b}"),
    (0x2F13, "\u{52f9}"), (0x2F14, "\u{5315}"), (0x2F15, "\u{531a}"), (0x2F16, "\u{5338}"),
    (0x2F17, "\u{5341}"), (0x2F18, "\u{535c}"), (0x2F19, "\u{5369}"), (0x2F1A, "\u{5382}"),
    (0x2F1B, "\u{53b6}"), (0x2F1C, "\u{53c8}"), (0x2F1D, "\u{53e3}"), (0x2F1E, "\u{56d7}"),
    (0x2F1F, "\u{571f}"), (0x2F20, "\u{58eb}"), (0x2F21, "\u{5902}"), (0x2F22, "\u{590a}"),
    (0x2F23, "\u{5915}"), (0x2F24, "\u{5927}"), (0x2F25, "\u{5973}"), (0x2F26, "\u{5b50}"),
    (0x2F27, "\u{5b80}"), (0x2F28, "\u{5bf8}"), (0x2F29, "\u{5c0f}"), (0x2F2A, "\u{5c22}"),
    (0x2F2B, "\u{5c38}"), (0x2F2C, "\u{5c6e}"), (0x2F2D, "\u{5c71}"), (0x2F2E, "\u{5ddb}"),
    (0x2F2F, "\u{5de5}"), (0x2F30, "\u{5df1}"), (0x2F31, "\u{5dfe}"), (0x2F32, "\u{5e72}"),
    (0x2F33, "\u{5e7a}"), (0x2F34, "\u{5e7f}"), (0x2F35, "\u{5ef4}"), (0x2F36, "\u{5efe}"),
    (0x2F37, "\u{5f0b}"), (0x2F38, "\u{5f13}"), (0x2F39, "\u{5f50}"), (0x2F3A, "\u{5f61}"),
    (0x2F3B, "\u{5f73}"), (0x2F3C, "\u{5fc3}"), (0x2F3D, "\u{6208}"), (0x2F3E, "\u{6236}"),
    (0x2F3F, "\u{624b}"), (0x2F40, "\u{652f}"), (0x2F41, "\u{6534}"), (0x2F42, "\u{6587}"),
    (0x2F43, "\u{6597}"), (0x2F44, "\u{65a4}"), (0x2F45, "\u{65b9}"), (0x2F46, "\u{65e0}"),
    (0x2F47, "\u{65e5}"), (0x2F48, "\u{66f0}"), (0x2F49, "\u{6708}"), (0x2F4A, "\u{6728}"),
    (0x2F4B, "\u{6b20}"), (0x2F4C, "\u{6b62}"), (0x2F4D, "\u{6b79}"), (0x2F4E, "\u{6bb3}"),
    (0x2F4F, "\u{6bcb}"), (0x2F50, "\u{6bd4}"), (0x2F51, "\u{6bdb}"), (0x2F52, "\u{6c0f}"),
    (0x2F53, "\u{6c14}"), (0x2F54, "\u{6c34}"), (0x2F55, "\u{706b}"), (0x2F56, "\u{722a}"),
    (0x2F57, "\u{7236}"), (0x2F58, "\u{723b}"), (0x2F59, "\u{723f}"), (0x2F5A, "\u{7247}"),
    (0x2F5B, "\u{7259}"), (0x2F5C, "\u{725b}"), (0x2F5D, "\u{72ac}"), (0x2F5E, "\u{7384}"),
    (0x2F5F, "\u{7389}"), (0x2F60, "\u{74dc}"), (0x2F61, "\u{74e6}"), (0x2F62, "\u{7518}"),
    (0x2F63, "\u{751f}"), (0x2F64, "\u{7528}"), (0x2F65, "\u{7530}"), (0x2F66, "\u{758b}"),
    (0x2F67, "\u{7592}"), (0x2F68, "\u{7676}"), (0x2F69, "\u{767d}"), (0x2F6A, "\u{76ae}"),
    (0x2F6B, "\u{76bf}"), (0x2F6C, "\u{76ee}"), (0x2F6D, "\u{77db}"), (0x2F6E, "\u{77e2}"),
    (0x2F6F, "\u{77f3}"), (0x2F70, "\u{793a}"), (0x2F71, "\u{79b8}"), (0x2F72, "\u{79be}"),
    (0x2F73, "\u{7a74}"), (0x2F74, "\u{7acb}"), (0x2F75, "\u{7af9}"), (0x2F76, "\u{7c73}"),
    (0x2F77, "\u{7cf8}"), (0x2F78, "\u{7f36}"), (0x2F79, "\u{7f51}"), (0x2F7A, "\u{7f8a}"),
    (0x2F7B, "\u{7fbd}"), (0x2F7C, "\u{8001}"), (0x2F7D, "\u{800c}"), (0x2F7E, "\u{8012}"),
    (0x2F7F, "\u{8033}"), (0x2F80, "\u{807f}"), (0x2F81, "\u{8089}"), (0x2F82, "\u{81e3}"),
    (0x2F83, "\u{81ea}"), (0x2F84, "\u{81f3}"), (0x2F85, "\u{81fc}"), (0x2F86, "\u{820c}"),
    (0x2F87, "\u{821b}"), (0x2F88, "\u{821f}"), (0x2F89, "\u{826e}"), (0x2F8A, "\u{8272}"),
    (0x2F8B, "\u{8278}"), (0x2F8C, "\u{864d}"), (0x2F8D, "\u{866b}"), (0x2F8E, "\u{8840}"),
    (0x2F8F, "\u{884c}"), (0x2F90, "\u{8863}"), (0x2F91, "\u{897e}"), (0x2F92, "\u{898b}"),
    (0x2F93, "\u{89d2}"), (0x2F94, "\u{8a00}"), (0x2F95, "\u{8c37}"), (0x2F96, "\u{8c46}"),
    (0x2F97, "\u{8c55}"), (0x2F98, "\u{8c78}"), (0x2F99, "\u{8c9d}"), (0x2F9A, "\u{8d64}"),
    (0x2F9B, "\u{8d70}"), (0x2F9C, "\u{8db3}"), (0x2F9D, "\u{8eab}"), (0x2F9E, "\u{8eca}"),
    (0x2F9F, "\u{8f9b}"), (0x2FA0, "\u{8fb0}"), (0x2FA1, "\u{8fb5}"), (0x2FA2, "\u{9091}"),
    (0x2FA3, "\u{9149}"), (0x2FA4, "\u{91c6}"), (0x2FA5, "\u{91cc}"), (0x2FA6, "\u{91d1}"),
    (0x2FA7, "\u{9577}"), (0x2FA8, "\u{9580}"), (0x2FA9, "\u{961c}"), (0x2FAA, "\u{96b6}"),
    (0x2FAB, "\u{96b9}"), (0x2FAC, "\u{96e8}"), (0x2FAD, "\u{9751}"), (0x2FAE, "\u{975e}"),
    (0x2FAF, "\u{9762}"), (0x2FB0, "\u{9769}"), (0x2FB1, "\u{97cb}"), (0x2FB2, "\u{97ed}"),
    (0x2FB3, "\u{97f3}"), (0x2FB4, "\u{9801}"), (0x2FB5, "\u{98a8}"), (0x2FB6, "\u{98db}"),
    (0x2FB7, "\u{98df}"), (0x2FB8, "\u{9996}"), (0x2FB9, "\u{9999}"), (0x2FBA, "\u{99ac}"),
    (0x2FBB, "\u{9aa8}"), (0x2FBC, "\u{9ad8}"), (0x2FBD, "\u{9adf}"), (0x2FBE, "\u{9b25}"),
    (0x2FBF, "\u{9b2f}"), (0x2FC0, "\u{9b32}"), (0x2FC1, "\u{9b3c}"), (0x2FC2, "\u{9b5a}"),
    (0x2FC3, "\u{9ce5}"), (0x2FC4, "\u{9e75}"), (0x2FC5, "\u{9e7f}"), (0x2FC6, "\u{9ea5}"),
    (0x2FC7, "\u{9ebb}"), (0x2FC8, "\u{9ec3}"), (0x2FC9, "\u{9ecd}"), (0x2FCA, "\u{9ed1}"),
    (0x2FCB, "\u{9ef9}"), (0x2FCC, "\u{9efd}"), (0x2FCD, "\u{9f0e}"), (0x2FCE, "\u{9f13}"),
    (0x2FCF, "\u{9f20}"), (0x2FD0, "\u{9f3b}"), (0x2FD1, "\u{9f4a}"), (0x2FD2, "\u{9f52}"),
    (0x2FD3, "\u{9f8d}"), (0x2FD4, "\u{9f9c}"), (0x2FD5, "\u{9fa0}"), (0x3000, " "),
    (0x3036, "\u{3012}"), (0x3038, "\u{5341}"), (0x3039, "\u{5344}"), (0x303A, "\u{5345}"),
    (0x304C, "\u{304b}\u{3099}"), (0x304E, "\u{304d}\u{3099}"), (0x3050, "\u{304f}\u{3099}"), (0x3052, "\u{3051}\u{3099}"),
    (0x3054, "\u{3053}\u{3099}"), (0x3056, "\u{3055}\u{3099}"), (0x3058, "\u{3057}\u{3099}"), (0x305A, "\u{3059}\u{3099}"),
    (0x305C, "\u{305b}\u{3099}"), (0x305E, "\u{305d}\u{3099}"), (0x3060, "\u{305f}\u{3099}"), (0x3062, "\u{3061}\u{3099}"),
    (0x3065, "\u{3064}\u{3099}"), (0x3067, "\u{3066}\u{3099}"), (0x3069, "\u{3068}\u{3099}"), (0x3070, "\u{306f}\u{3099}"),
    (0x3071, "\u{306f}\u{309a}"), (0x3073, "\u{3072}\u{3099}"), (0x3074, "\u{3072}\u{309a}"), (0x3076, "\u{3075}\u{3099}"),
    (0x3077, "\u{3075}\u{309a}"), (0x3079, "\u{3078}\u{3099}"), (0x307A, "\u{3078}\u{309a}"), (0x307C, "\u{307b}\u{3099}"),
    (0x307D, "\u{307b}\u{309a}"), (0x3094, "\u{3046}\u{3099}"), (0x309B, " \u{3099}"), (0x309C, " \u{309a}"),
    (0x309E, "\u{309d}\u{3099}"), (0x309F, "\u{3088}\u{308a}"), (0x30AC, "\u{30ab}\u{3099}"), (0x30AE, "\u{30ad}\u{3099}"),
    (0x30B0, "\u{30af}\u{3099}"), (0x30B2, "\u{30b1}\u{3099}"), (0x30B4, "\u{30b3}\u{3099}"), (0x30B6, "\u{30b5}\u{3099}"),
    (0x30B8, "\u{30b7}\u{3099}"), (0x30BA, "\u{30b9}\u{3099}"), (0x30BC, "\u{30bb}\u{3099}"), (0x30BE, "\u{30bd}\u{3099}"),
    (0x30C0, "\u{30bf}\u{3099}"), (0x30C2, "\u{30c1}\u{3099}"), (0x30C5, "\u{30c4}\u{3099}"), (0x30C7, "\u{30c6}\u{3099}"),
    (0x30C9, "\u{30c8}\u{3099}"), (0x30D0, "\u{30cf}\u{3099}"), (0x30D1, "\u{30cf}\u{309a}"), (0x30D3, "\u{30d2}\u{3099}"),
    (0x30D4, "\u{30d2}\u{309a}"), (0x30D6, "\u{30d5}\u{3099}"), (0x30D7, "\u{30d5}\u{309a}"), (0x30D9, "\u{30d8}\u{3099}"),
    (0x30DA, "\u{30d8}\u{309a}"), (0x30DC, "\u{30db}\u{3099}"), (0x30DD, "\u{30db}\u{309a}"), (0x30F4, "\u{30a6}\u{3099}"),
    (0x30F7, "\u{30ef}\u{3099}"), (0x30F8, "\u{30f0}\u{3099}"), (0x30F9, "\u{30f1}\u{3099}"), (0x30FA, "\u{30f2}\u{3099}"),
    (0x30FE, "\u{30fd}\u{3099}"), (0x30FF, "\u{30b3}\u{30c8}"), (0x3131, "\u{1100}"), (0x3132, "\u{1101}"),
    (0x3133, "\u{11aa}"), (0x3134, "\u{1102}"), (0x3135, "\u{11ac}"), (0x3136, "\u{11ad}"),
    (0x3137, "\u{1103}"), (0x3138, "\u{1104}"), (0x3139, "\u{1105}"), (0x313A, "\u{11b0}"),
    (0x313B, "\u{11b1}"), (0x313C, "\u{11b2}"), (0x313D, "\u{11b3}"), (0x313E, "\u{11b4}"),
    (0x313F, "\u{11b5}"), (0x3140, "\u{111a}"), (0x3141, "\u{1106}"), (0x3142, "\u{1107}"),
    (0x3143, "\u{1108}"), (0x3144, "\u{1121}"), (0x3145, "\u{1109}"), (0x3146, "\u{110a}"),
    (0x3147, "\u{110b}"), (0x3148, "\u{110c}"), (0x3149, "\u{110d}"), (0x314A, "\u{110e}"),
    (0x314B, "\u{110f}"), (0x314C, "\u{1110}"), (0x314D, "\u{1111}"), (0x314E, "\u{1112}"),
    (0x314F, "\u{1161}"), (0x3150, "\u{1162}"), (0x3151, "\u{1163}"), (0x3152, "\u{1164}"),
    (0x3153, "\u{1165}"), (0x3154, "\u{1166}"), (0x3155, "\u{1167}"), (0x3156, "\u{1168}"),
    (0x3157, "\u{1169}"), (0x3158, "\u{116a}"), (0x3159, "\u{116b}"), (0x315A, "\u{116c}"),
    (0x315B, "\u{116d}"), (0x315C, "\u{116e}"), (0x315D, "\u{116f}"), (0x315E, "\u{1170}"),
    (0x315F, "\u{1171}"), (0x3160, "\u{1172}"), (0x3161, "\u{1173}"), (0x3162, "\u{1174}"),
    (0x3163, "\u{1175}"), (0x3164, "\u{1160}"), (0x3165, "\u{1114}"), (0x3166, "\u{1115}"),
    (0x3167, "\u{11c7}"), (0x3168, "\u{11c8}"), (0x3169, "\u{11cc}"), (0x316A, "\u{11ce}"),
    (0x316B, "\u{11d3}"), (0x316C, "\u{11d7}"), (0x316D, "\u{11d9}"), (0x316E, "\u{111c}"),
    (0x316F, "\u{11dd}"), (0x3170, "\u{11df}"), (0x3171, "\u{111d}"), (0x3172, "\u{111e}"),
    (0x3173, "\u{1120}"), (0x3174, "\u{1122}"), (0x3175, "\u{1123}"), (0x3176, "\u{1127}"),
    (0x3177, "\u{1129}"), (0x3178, "\u{112b}"), (0x3179, "\u{112c}"), (0x317A, "\u{112d}"),
    (0x317B, "\u{112e}"), (0x317C, "\u{112f}"), (0x317D, "\u{1132}"), (0x317E, "\u{1136}"),
    (0x317F, "\u{1140}"), (0x3180, "\u{1147}"), (0x3181, "\u{114c}"), (0x3182, "\u{11f1}"),
    (0x3183, "\u{11f2}"), (0x3184, "\u{1157}"), (0x3185, "\u{1158}"), (0x3186, "\u{1159}"),
    (0x3187, "\u{1184}"), (0x3188, "\u{1185}"), (0x3189, "\u{1188}"), (0x318A, "\u{1191}"),
    (0x318B, "\u{1192}"), (0x318C, "\u{1194}"), (0x318D, "\u{119e}"), (0x318E, "\u{11a1}"),
    (0x3192, "\u{4e00}"), (0x3193, "\u{4e8c}"), (0x3194, "\u{4e09}"), (0x3195, "\u{56db}"),
    (0x3196, "\u{4e0a}"), (0x3197, "\u{4e2d}"), (0x3198, "\u{4e0b}"), (0x3199, "\u{7532}"),
    (0x319A, "\u{4e59}"), (0x319B, "\u{4e19}"), (0x319C, "\u{4e01}"), (0x319D, "\u{5929}"),
    (0x319E, "\u{5730}"), (0x319F, "\u{4eba}"), (0x3200, "(\u{1100})"), (0x3201, "(\u{1102})"),
    (0x3202, "(\u{1103})"), (0x3203, "(\u{1105})"), (0x3204, "(\u{1106})"), (0x3205, "(\u{1107})"),
    (0x3206, "(\u{1109})"), (0x3207, "(\u{110b})"), (0x3208, "(\u{110c})"), (0x3209, "(\u{110e})"),
    (0x320A, "(\u{110f})"), (0x320B, "(\u{1110})"), (0x320C, "(\u{1111})"), (0x320D, "(\u{1112})"),
    (0x320E, "(\u{1100}\u{1161})"), (0x320F, "(\u{1102}\u{1161})"), (0x3210, "(\u{1103}\u{1161})"), (0x3211, "(\u{1105}\u{1161})"),
    (0x3212, "(\u{1106}\u{1161})"), (0x3213, "(\u{1107}\u{1161})"), (0x3214, "(\u{1109}\u{1161})"), (0x3215, "(\u{110b}\u{1161})"),
    (0x3216, "(\u{110c}\u{1161})"), (0x3217, "(\u{110e}\u{1161})"), (0x3218, "(\u{110f}\u{1161})"), (0x3219, "(\u{1110}\u{1161})"),
    (0x321A, "(\u{1111}\u{1161})"), (0x321B, "(\u{1112}\u{1161})"), (0x321C, "(\u{110c}\u{116e})"), (0x321D, "(\u{110b}\u{1169}\u{110c}\u{1165}\u{11ab})"),
    (0x321E, "(\u{110b}\u{1169}\u{1112}\u{116e})"), (0x3220, "(\u{4e00})"), (0x3221, "(\u{4e8c})"), (0x3222, "(\u{4e09})"),
    (0x3223, "(\u{56db})"), (0x3224, "(\u{4e94})"), (0x3225, "(\u{516d})"), (0x3226, "(\u{4e03})"),
    (0x3227, "(\u{516b})"), (0x3228, "(\u{4e5d})"), (0x3229, "(\u{5341})"), (0x322A, "(\u{6708})"),
    (0x322B, "(\u{706b})"), (0x322C, "(\u{6c34})"), (0x322D, "(\u{6728})"), (0x322E, "(\u{91d1})"),
    (0x322F, "(\u{571f})"), (0x3230, "(\u{65e5})"), (0x3231, "(\u{682a})"), (0x3232, "(\u{6709})"),
    (0x3233, "(\u{793e})"), (0x3234, "(\u{540d})"), (0x3235, "(\u{7279})"), (0x3236, "(\u{8ca1})"),
    (0x3237, "(\u{795d})"), (0x3238, "(\u{52b4})"), (0x3239, "(\u{4ee3})"), (0x323A, "(\u{547c})"),
    (0x323B, "(\u{5b66})"), (0x323C, "(\u{76e3})"), (0x323D, "(\u{4f01})"), (0x323E, "(\u{8cc7})"),
    (0x323F, "(\u{5354})"), (0x3240, "(\u{796d})"), (0x3241, "(\u{4f11})"), (0x3242, "(\u{81ea})"),
    (0x3243, "(\u{81f3})"), (0x3244, "\u{554f}"), (0x3245, "\u{5e7c}"), (0x3246, "\u{6587}"),
    (0x3247, "\u{7b8f}"), (0x3250, "PTE"), (0x3251, "21"), (0x3252, "22"),
    (0x3253, "23"), (0x3254, "24"), (0x3255, "25"), (0x3256, "26"),
    (0x3257, "27"), (0x3258, "28"), (0x3259, "29"), (0x325A, "30"),
    (0x325B, "31"), (0x325C, "32"), (0x325D, "33"), (0x325E, "34"),
    (0x325F, "35"), (0x3260, "\u{1100}"), (0x3261, "\u{1102}"), (0x3262, "\u{1103}"),
    (0x3263, "\u{1105}"), (0x3264, "\u{1106}"), (0x3265, "\u{1107}"), (0x3266, "\u{1109}"),
    (0x3267, "\u{110b}"), (0x3268, "\u{110c}"), (0x3269, "\u{110e}"), (0x326A, "\u{110f}"),
    (0x326B, "\u{1110}"), (0x326C, "\u{1111}"), (0x326D, "\u{1112}"), (0x326E, "\u{1100}\u{1161}"),
    (0x326F, "\u{1102}\u{1161}"), (0x3270, "\u{1103}\u{1161}"), (0x3271, "\u{1105}\u{1161}"), (0x3272, "\u{1106}\u{1161}"),
    (0x3273, "\u{1107}\u{1161}"), (0x3274, "\u{1109}\u{1161}"), (0x3275, "\u{110b}\u{1161}"), (0x3276, "\u{110c}\u{1161}"),
    (0x3277, "\u{110e}\u{1161}"), (0x3278, "\u{110f}\u{1161}"), (0x3279, "\u{1110}\u{1161}"), (0x327A, "\u{1111}\u{1161}"),
    (0x327B, "\u{1112}\u{1161}"), (0x327C, "\u{110e}\u{1161}\u{11b7}\u{1100}\u{1169}"), (0x327D, "\u{110c}\u{116e}\u{110b}\u{1174}"), (0x327E, "\u{110b}\u{116e}"),
    (0x3280, "\u{4e00}"), (0x3281, "\u{4e8c}"), (0x3282, "\u{4e09}"), (0x3283, "\u{56db}"),
    (0x3284, "\u{4e94}"), (0x3285, "\u{516d}"), (0x3286, "\u{4e03}"), (0x3287, "\u{516b}"),
    (0x3288, "\u{4e5d}"), (0x3289, "\u{5341}"), (0x328A, "\u{6708}"), (0x328B, "\u{706b}"),
    (0x328C, "\u{6c34}"), (0x328D, "\u{6728}"), (0x328E, "\u{91d1}"), (0x328F, "\u{571f}"),
    (0x3290, "\u{65e5}"), (0x3291, "\u{682a}"), (0x3292, "\u{6709}"), (0x3293, "\u{793e}"),
    (0x3294, "\u{540d}"), (0x3295, "\u{7279}"), (0x3296, "\u{8ca1}"), (0x3297, "\u{795d}"),
    (0x3298, "\u{52b4}"), (0x3299, "\u{79d8}"), (0x329A, "\u{7537}"), (0x329B, "\u{5973}"),
    (0x329C, "\u{9069}"), (0x329D, "\u{512a}"), (0x329E, "\u{5370}"), (0x329F, "\u{6ce8}"),
    (0x32A0, "\u{9805}"), (0x32A1, "\u{4f11}"), (0x32A2, "\u{5199}"), (0x32A3, "\u{6b63}"),
    (0x32A4, "\u{4e0a}"), (0x32A5, "\u{4e2d}"), (0x32A6, "\u{4e0b}"), (0x32A7, "\u{5de6}"),
    (0x32A8, "\u{53f3}"), (0x32A9, "\u{533b}"), (0x32AA, "\u{5b97}"), (0x32AB, "\u{5b66}"),
    (0x32AC, "\u{76e3}"), (0x32AD, "\u{4f01}"), (0x32AE, "\u{8cc7}"), (0x32AF, "\u{5354}"),
    (0x32B0, "\u{591c}"), (0x32B1, "36"), (0x32B2, "37"), (0x32B3, "38"),
    (0x32B4, "39"), (0x32B5, "40"), (0x32B6, "41"), (0x32B7, "42"),
    (0x32B8, "43"), (0x32B9, "44"), (0x32BA, "45"), (0x32BB, "46"),
    (0x32BC, "47"), (0x32BD, "48"), (0x32BE, "49"), (0x32BF, "50"),
    (0x32C0, "1\u{6708}"), (0x32C1, "2\u{6708}"), (0x32C2, "3\u{6708}"), (0x32C3, "4\u{6708}"),
    (0x32C4, "5\u{6708}"), (0x32C5, "6\u{6708}"), (0x32C6, "7\u{6708}"), (0x32C7, "8\u{6708}"),
    (0x32C8, "9\u{6708}"), (0x32C9, "10\u{6708}"), (0x32CA, "11\u{6708}"), (0x32CB, "12\u{6708}"),
    (0x32CC, "Hg"), (0x32CD, "erg"), (0x32CE, "eV"), (0x32CF, "LTD"),
    (0x32D0, "\u{30a2}"), (0x32D1, "\u{30a4}"), (0x32D2, "\u{30a6}"), (0x32D3, "\u{30a8}"),
    (0x32D4, "\u{30aa}"), (0x32D5, "\u{30ab}"), (0x32D6, "\u{30ad}"), (0x32D7, "\u{30af}"),
    (0x32D8, "\u{30b1}"), (0x32D9, "\u{30b3}"), (0x32DA, "\u{30b5}"), (0x32DB, "\u{30b7}"),
    (0x32DC, "\u{30b9}"), (0x32DD, "\u{30bb}"), (0x32DE, "\u{30bd}"), (0x32DF, "\u{30bf}"),
    (0x32E0, "\u{30c1}"), (0x32E1, "\u{30c4}"), (0x32E2, "\u{30c6}"), (0x32E3, "\u{30c8}"),
    (0x32E4, "\u{30ca}"), (0x32E5, "\u{30cb}"), (0x32E6, "\u{30cc}"), (0x32E7, "\u{30cd}"),
    (0x32E8, "\u{30ce}"), (0x32E9, "\u{30cf}"), (0x32EA, "\u{30d2}"), (0x32EB, "\u{30d5}"),
    (0x32EC, "\u{30d8}"), (0x32ED, "\u{30db}"), (0x32EE, "\u{30de}"), (0x32EF, "\u{30df}"),
    (0x32F0, "\u{30e0}"), (0x32F1, "\u{30e1}"), (0x32F2, "\u{30e2}"), (0x32F3, "\u{30e4}"),
    (0x32F4, "\u{30e6}"), (0x32F5, "\u{30e8}"), (0x32F6, "\u{30e9}"), (0x32F7, "\u{30ea}"),
    (0x32F8, "\u{30eb}"), (0x32F9, "\u{30ec}"), (0x32FA, "\u{30ed}"), (0x32FB, "\u{30ef}"),
    (0x32FC, "\u{30f0}"), (0x32FD, "\u{30f1}"), (0x32FE, "\u{30f2}"), (0x32FF, "\u{4ee4}\u{548c}"),
    (0x3300, "\u{30a2}\u{30cf}\u{309a}\u{30fc}\u{30c8}"), (0x3301, "\u{30a2}\u{30eb}\u{30d5}\u{30a1}"), (0x3302, "\u{30a2}\u{30f3}\u{30d8}\u{309a}\u{30a2}"), (0x3303, "\u{30a2}\u{30fc}\u{30eb}"),
    (0x3304, "\u{30a4}\u{30cb}\u{30f3}\u{30af}\u{3099}"), (0x3305, "\u{30a4}\u{30f3}\u{30c1}"), (0x3306, "\u{30a6}\u{30a9}\u{30f3}"), (0x3307, "\u{30a8}\u{30b9}\u{30af}\u{30fc}\u{30c8}\u{3099}"),
    (0x3308, "\u{30a8}\u{30fc}\u{30ab}\u{30fc}"), (0x3309, "\u{30aa}\u{30f3}\u{30b9}"), (0x330A, "\u{30aa}\u{30fc}\u{30e0}"), (0x330B, "\u{30ab}\u{30a4}\u{30ea}"),
    (0x330C, "\u{30ab}\u{30e9}\u{30c3}\u{30c8}"), (0x330D, "\u{30ab}\u{30ed}\u{30ea}\u{30fc}"), (0x330E, "\u{30ab}\u{3099}\u{30ed}\u{30f3}"), (0x330F, "\u{30ab}\u{3099}\u{30f3}\u{30de}"),
    (0x3310, "\u{30ad}\u{3099}\u{30ab}\u{3099}"), (0x3311, "\u{30ad}\u{3099}\u{30cb}\u{30fc}"), (0x3312, "\u{30ad}\u{30e5}\u{30ea}\u{30fc}"), (0x3313, "\u{30ad}\u{3099}\u{30eb}\u{30bf}\u{3099}\u{30fc}"),
    (0x3314, "\u{30ad}\u{30ed}"), (0x3315, "\u{30ad}\u{30ed}\u{30af}\u{3099}\u{30e9}\u{30e0}"), (0x3316, "\u{30ad}\u{30ed}\u{30e1}\u{30fc}\u{30c8}\u{30eb}"), (0x3317, "\u{30ad}\u{30ed}\u{30ef}\u{30c3}\u{30c8}"),
    (0x3318, "\u{30af}\u{3099}\u{30e9}\u{30e0}"), (0x3319, "\u{30af}\u{3099}\u{30e9}\u{30e0}\u{30c8}\u{30f3}"), (0x331A, "\u{30af}\u{30eb}\u{30bb}\u{3099}\u{30a4}\u{30ed}"), (0x331B, "\u{30af}\u{30ed}\u{30fc}\u{30cd}"),
    (0x331C, "\u{30b1}\u{30fc}\u{30b9}"), (0x331D, "\u{30b3}\u{30eb}\u{30ca}"), (0x331E, "\u{30b3}\u{30fc}\u{30db}\u{309a}"), (0x331F, "\u{30b5}\u{30a4}\u{30af}\u{30eb}"),
    (0x3320, "\u{30b5}\u{30f3}\u{30c1}\u{30fc}\u{30e0}"), (0x3321, "\u{30b7}\u{30ea}\u{30f3}\u{30af}\u{3099}"), (0x3322, "\u{30bb}\u{30f3}\u{30c1}"), (0x3323, "\u{30bb}\u{30f3}\u{30c8}"),
    (0x3324, "\u{30bf}\u{3099}\u{30fc}\u{30b9}"), (0x3325, "\u{30c6}\u{3099}\u{30b7}"), (0x3326, "\u{30c8}\u{3099}\u{30eb}"), (0x3327, "\u{30c8}\u{30f3}"),
    (0x3328, "\u{30ca}\u{30ce}"), (0x3329, "\u{30ce}\u{30c3}\u{30c8}"), (0x332A, "\u{30cf}\u{30a4}\u{30c4}"), (0x332B, "\u{30cf}\u{309a}\u{30fc}\u{30bb}\u{30f3}\u{30c8}"),
    (0x332C, "\u{30cf}\u{309a}\u{30fc}\u{30c4}"), (0x332D, "\u{30cf}\u{3099}\u{30fc}\u{30ec}\u{30eb}"), (0x332E, "\u{30d2}\u{309a}\u{30a2}\u{30b9}\u{30c8}\u{30eb}"), (0x332F, "\u{30d2}\u{309a}\u{30af}\u{30eb}"),
    (0x3330, "\u{30d2}\u{309a}\u{30b3}"), (0x3331, "\u{30d2}\u{3099}\u{30eb}"), (0x3332, "\u{30d5}\u{30a1}\u{30e9}\u{30c3}\u{30c8}\u{3099}"), (0x3333, "\u{30d5}\u{30a3}\u{30fc}\u{30c8}"),
    (0x3334, "\u{30d5}\u{3099}\u{30c3}\u{30b7}\u{30a7}\u{30eb}"), (0x3335, "\u{30d5}\u{30e9}\u{30f3}"), (0x3336, "\u{30d8}\u{30af}\u{30bf}\u{30fc}\u{30eb}"), (0x3337, "\u{30d8}\u{309a}\u{30bd}"),
    (0x3338, "\u{30d8}\u{309a}\u{30cb}\u{30d2}"), (0x3339, "\u{30d8}\u{30eb}\u{30c4}"), (0x333A, "\u{30d8}\u{309a}\u{30f3}\u{30b9}"), (0x333B, "\u{30d8}\u{309a}\u{30fc}\u{30b7}\u{3099}"),
    (0x333C, "\u{30d8}\u{3099}\u{30fc}\u{30bf}"), (0x333D, "\u{30db}\u{309a}\u{30a4}\u{30f3}\u{30c8}"), (0x333E, "\u{30db}\u{3099}\u{30eb}\u{30c8}"), (0x333F, "\u{30db}\u{30f3}"),
    (0x3340, "\u{30db}\u{309a}\u{30f3}\u{30c8}\u{3099}"), (0x3341, "\u{30db}\u{30fc}\u{30eb}"), (0x3342, "\u{30db}\u{30fc}\u{30f3}"), (0x3343, "\u{30de}\u{30a4}\u{30af}\u{30ed}"),
    (0x3344, "\u{30de}\u{30a4}\u{30eb}"), (0x3345, "\u{30de}\u{30c3}\u{30cf}"), (0x3346, "\u{30de}\u{30eb}\u{30af}"), (0x3347, "\u{30de}\u{30f3}\u{30b7}\u{30e7}\u{30f3}"),
    (0x3348, "\u{30df}\u{30af}\u{30ed}\u{30f3}"), (0x3349, "\u{30df}\u{30ea}"), (0x334A, "\u{30df}\u{30ea}\u{30cf}\u{3099}\u{30fc}\u{30eb}"), (0x334B, "\u{30e1}\u{30ab}\u{3099}"),
    (0x334C, "\u{30e1}\u{30ab}\u{3099}\u{30c8}\u{30f3}"), (0x334D, "\u{30e1}\u{30fc}\u{30c8}\u{30eb}"), (0x334E, "\u{30e4}\u{30fc}\u{30c8}\u{3099}"), (0x334F, "\u{30e4}\u{30fc}\u{30eb}"),
    (0x3350, "\u{30e6}\u{30a2}\u{30f3}"), (0x3351, "\u{30ea}\u{30c3}\u{30c8}\u{30eb}"), (0x3352, "\u{30ea}\u{30e9}"), (0x3353, "\u{30eb}\u{30d2}\u{309a}\u{30fc}"),
    (0x3354, "\u{30eb}\u{30fc}\u{30d5}\u{3099}\u{30eb}"), (0x3355, "\u{30ec}\u{30e0}"), (0x3356, "\u{30ec}\u{30f3}\u{30c8}\u{30b1}\u{3099}\u{30f3}"), (0x3357, "\u{30ef}\u{30c3}\u{30c8}"),
    (0x3358, "0\u{70b9}"), (0x3359, "1\u{70b9}"), (0x335A, "2\u{70b9}"), (0x335B, "3\u{70b9}"),
    (0x335C, "4\u{70b9}"), (0x335D, "5\u{70b9}"), (0x335E, "6\u{70b9}"), (0x335F, "7\u{70b9}"),
    (0x3360, "8\u{70b9}"), (0x3361, "9\u{70b9}"), (0x3362, "10\u{70b9}"), (0x3363, "11\u{70b9}"),
    (0x3364, "12\u{70b9}"), (0x3365, "13\u{70b9}"), (0x3366, "14\u{70b9}"), (0x3367, "15\u{70b9}"),
    (0x3368, "16\u{70b9}"), (0x3369, "17\u{70b9}"), (0x336A, "18\u{70b9}"), (0x336B, "19\u{70b9}"),
    (0x336C, "20\u{70b9}"), (0x336D, "21\u{70b9}"), (0x336E, "22\u{70b9}"), (0x336F, "23\u{70b9}"),
    (0x3370, "24\u{70b9}"), (0x3371, "hPa"), (0x3372, "da"), (0x3373, "AU"),
    (0x3374, "bar"), (0x3375, "oV"), (0x3376, "pc"), (0x3377, "dm"),
    (0x3378, "dm2"), (0x3379, "dm3"), (0x337A, "IU"), (0x337B, "\u{5e73}\u{6210}"),
    (0x337C, "\u{662d}\u{548c}"), (0x337D, "\u{5927}\u{6b63}"), (0x337E, "\u{660e}\u{6cbb}"), (0x337F, "\u{682a}\u{5f0f}\u{4f1a}\u{793e}"),
    (0x3380, "pA"), (0x3381, "nA"), (0x3382, "\u{3bc}A"), (0x3383, "mA"),
    (0x3384, "kA"), (0x3385, "KB"), (0x3386, "MB"), (0x3387, "GB"),
    (0x3388, "cal"), (0x3389, "kcal"), (0x338A, "pF"), (0x338B, "nF"),
    (0x338C, "\u{3bc}F"), (0x338D, "\u{3bc}g"), (0x338E, "mg"), (0x338F, "kg"),
    (0x3390, "Hz"), (0x3391, "kHz"), (0x3392, "MHz"), (0x3393, "GHz"),
    (0x3394, "THz"), (0x3395, "\u{3bc}l"), (0x3396, "ml"), (0x3397, "dl"),
    (0x3398, "kl"), (0x3399, "fm"), (0x339A, "nm"), (0x339B, "\u{3bc}m"),
    (0x339C, "mm"), (0x339D, "cm"), (0x339E, "km"), (0x339F, "mm2"),
    (0x33A0, "cm2"), (0x33A1, "m2"), (0x33A2, "km2"), (0x33A3, "mm3"),
    (0x33A4, "cm3"), (0x33A5, "m3"), (0x33A6, "km3"), (0x33A7, "m\u{2215}s"),
    (0x33A8, "m\u{2215}s2"), (0x33A9, "Pa"), (0x33AA, "kPa"), (0x33AB, "MPa"),
    (0x33AC, "GPa"), (0x33AD, "rad"), (0x33AE, "rad\u{2215}s"), (0x33AF, "rad\u{2215}s2"),
    (0x33B0, "ps"), (0x33B1, "ns"), (0x33B2, "\u{3bc}s"), (0x33B3, "ms"),
    (0x33B4, "pV"), (0x33B5, "nV"), (0x33B6, "\u{3bc}V"), (0x33B7, "mV"),
    (0x33B8, "kV"), (0x33B9, "MV"), (0x33BA, "pW"), (0x33BB, "nW"),
    (0x33BC, "\u{3bc}W"), (0x33BD, "mW"), (0x33BE, "kW"), (0x33BF, "MW"),
    (0x33C0, "k\u{3a9}"), (0x33C1, "M\u{3a9}"), (0x33C2, "a.m."), (0x33C3, "Bq"),
    (0x33C4, "cc"), (0x33C5, "cd"), (0x33C6, "C\u{2215}kg"), (0x33C7, "Co."),
    (0x33C8, "dB"), (0x33C9, "Gy"), (0x33CA, "ha"), (0x33CB, "HP"),
    (0x33CC, "in"), (0x33CD, "KK"), (0x33CE, "KM"), (0x33CF, "kt"),
    (0x33D0, "lm"), (0x33D1, "ln"), (0x33D2, "log"), (0x33D3, "lx"),
    (0x33D4, "mb"), (0x33D5, "mil"), (0x33D6, "mol"), (0x33D7, "PH"),
    (0x33D8, "p.m."), (0x33D9, "PPM"), (0x33DA, "PR"), (0x33DB, "sr"),
    (0x33DC, "Sv"), (0x33DD, "Wb"), (0x33DE, "V\u{2215}m"), (0x33DF, "A\u{2215}m"),
    (0x33E0, "1\u{65e5}"), (0x33E1, "2\u{65e5}"), (0x33E2, "3\u{65e5}"), (0x33E3, "4\u{65e5}"),
    (0x33E4, "5\u{65e5}"), (0x33E5, "6\u{65e5}"), (0x33E6, "7\u{65e5}"), (0x33E7, "8\u{65e5}"),
    (0x33E8, "9\u{65e5}"), (0x33E9, "10\u{65e5}"), (0x33EA, "11\u{65e5}"), (0x33EB, "12\u{65e5}"),
    (0x33EC, "13\u{65e5}"), (0x33ED, "14\u{65e5}"), (0x33EE, "15\u{65e5}"), (0x33EF, "16\u{65e5}"),
    (0x33F0, "17\u{65e5}"), (0x33F1, "18\u{65e5}"), (0x33F2, "19\u{65e5}"), (0x33F3, "20\u{65e5}"),
    (0x33F4, "21\u{65e5}"), (0x33F5, "22\u{65e5}"), (0x33F6, "23\u{65e5}"), (0x33F7, "24\u{65e5}"),
    (0x33F8, "25\u{65e5}"), (0x33F9, "26\u{65e5}"), (0x33FA, "27\u{65e5}"), (0x33FB, "28\u{65e5}"),
    (0x33FC, "29\u{65e5}"), (0x33FD, "30\u{65e5}"), (0x33FE, "31\u{65e5}"), (0x33FF, "gal"),
    (0xA69C, "\u{44a}"), (0xA69D, "\u{44c}"), (0xA770, "\u{a76f}"), (0xA7F2, "C"),
    (0xA7F3, "F"), (0xA7F4, "Q"), (0xA7F8, "\u{126}"), (0xA7F9, "\u{153}"),
    (0xAB5C, "\u{a727}"), (0xAB5D, "\u{ab37}"), (0xAB5E, "\u{26b}"), (0xAB5F, "\u{ab52}"),
    (0xAB69, "\u{28d}"), (0xF900, "\u{8c48}"), (0xF901, "\u{66f4}"), (0xF902, "\u{8eca}"),
    (0xF903, "\u{8cc8}"), (0xF904, "\u{6ed1}"), (0xF905, "\u{4e32}"), (0xF906, "\u{53e5}"),
    (0xF907, "\u{9f9c}"), (0xF908, "\u{9f9c}"), (0xF909, "\u{5951}"), (0xF90A, "\u{91d1}"),
    (0xF90B, "\u{5587}"), (0xF90C, "\u{5948}"), (0xF90D, "\u{61f6}"), (0xF90E, "\u{7669}"),
    (0xF90F, "\u{7f85}"), (0xF910, "\u{863f}"), (0xF911, "\u{87ba}"), (0xF912, "\u{88f8}"),
    (0xF913, "\u{908f}"), (0xF914, "\u{6a02}"), (0xF915, "\u{6d1b}"), (0xF916, "\u{70d9}"),
    (0xF917, "\u{73de}"), (0xF918, "\u{843d}"), (0xF919, "\u{916a}"), (0xF91A, "\u{99f1}"),
    (0xF91B, "\u{4e82}"), (0xF91C, "\u{5375}"), (0xF91D, "\u{6b04}"), (0xF91E, "\u{721b}"),
    (0xF91F, "\u{862d}"), (0xF920, "\u{9e1e}"), (0xF921, "\u{5d50}"), (0xF922, "\u{6feb}"),
    (0xF923, "\u{85cd}"), (0xF924, "\u{8964}"), (0xF925, "\u{62c9}"), (0xF926, "\u{81d8}"),
    (0xF927, "\u{881f}"), (0xF928, "\u{5eca}"), (0xF929, "\u{6717}"), (0xF92A, "\u{6d6a}"),
    (0xF92B, "\u{72fc}"), (0xF92C, "\u{90ce}"), (0xF92D, "\u{4f86}"), (0xF92E, "\u{51b7}"),
    (0xF92F, "\u{52de}"), (0xF930, "\u{64c4}"), (0xF931, "\u{6ad3}"), (0xF932, "\u{7210}"),
    (0xF933, "\u{76e7}"), (0xF934, "\u{8001}"), (0xF935, "\u{8606}"), (0xF936, "\u{865c}"),
    (0xF937, "\u{8def}"), (0xF938, "\u{9732}"), (0xF939, "\u{9b6f}"), (0xF93A, "\u{9dfa}"),
    (0xF93B, "\u{788c}"), (0xF93C, "\u{797f}"), (0xF93D, "\u{7da0}"), (0xF93E, "\u{83c9}"),
    (0xF93F, "\u{9304}"), (0xF940, "\u{9e7f}"), (0xF941, "\u{8ad6}"), (0xF942, "\u{58df}"),
    (0xF943, "\u{5f04}"), (0xF944, "\u{7c60}"), (0xF945, "\u{807e}"), (0xF946, "\u{7262}"),
    (0xF947, "\u{78ca}"), (0xF948, "\u{8cc2}"), (0xF949, "\u{96f7}"), (0xF94A, "\u{58d8}"),
    (0xF94B, "\u{5c62}"), (0xF94C, "\u{6a13}"), (0xF94D, "\u{6dda}"), (0xF94E, "\u{6f0f}"),
    (0xF94F, "\u{7d2f}"), (0xF950, "\u{7e37}"), (0xF951, "\u{964b}"), (0xF952, "\u{52d2}"),
    (0xF953, "\u{808b}"), (0xF954, "\u{51dc}"), (0xF955, "\u{51cc}"), (0xF956, "\u{7a1c}"),
    (0xF957, "\u{7dbe}"), (0xF958, "\u{83f1}"), (0xF959, "\u{9675}"), (0xF95A, "\u{8b80}"),
    (0xF95B, "\u{62cf}"), (0xF95C, "\u{6a02}"), (0xF95D, "\u{8afe}"), (0xF95E, "\u{4e39}"),
    (0xF95F, "\u{5be7}"), (0xF960, "\u{6012}"), (0xF961, "\u{7387}"), (0xF962, "\u{7570}"),
    (0xF963, "\u{5317}"), (0xF964, "\u{78fb}"), (0xF965, "\u{4fbf}"), (0xF966, "\u{5fa9}"),
    (0xF967, "\u{4e0d}"), (0xF968, "\u{6ccc}"), (0xF969, "\u{6578}"), (0xF96A, "\u{7d22}"),
    (0xF96B, "\u{53c3}"), (0xF96C, "\u{585e}"), (0xF96D, "\u{7701}"), (0xF96E, "\u{8449}"),
    (0xF96F, "\u{8aaa}"), (0xF970, "\u{6bba}"), (0xF971, "\u{8fb0}"), (0xF972, "\u{6c88}"),
    (0xF973, "\u{62fe}"), (0xF974, "\u{82e5}"), (0xF975, "\u{63a0}"), (0xF976, "\u{7565}"),
    (0xF977, "\u{4eae}"), (0xF978, "\u{5169}"), (0xF979, "\u{51c9}"), (0xF97A, "\u{6881}"),
    (0xF97B, "\u{7ce7}"), (0xF97C, "\u{826f}"), (0xF97D, "\u{8ad2}"), (0xF97E, "\u{91cf}"),
    (0xF97F, "\u{52f5}"), (0xF980, "\u{5442}"), (0xF981, "\u{5973}"), (0xF982, "\u{5eec}"),
    (0xF983, "\u{65c5}"), (0xF984, "\u{6ffe}"), (0xF985, "\u{792a}"), (0xF986, "\u{95ad}"),
    (0xF987, "\u{9a6a}"), (0xF988, "\u{9e97}"), (0xF989, "\u{9ece}"), (0xF98A, "\u{529b}"),
    (0xF98B, "\u{66c6}"), (0xF98C, "\u{6b77}"), (0xF98D, "\u{8f62}"), (0xF98E, "\u{5e74}"),
    (0xF98F, "\u{6190}"), (0xF990, "\u{6200}"), (0xF991, "\u{649a}"), (0xF992, "\u{6f23}"),
    (0xF993, "\u{7149}"), (0xF994, "\u{7489}"), (0xF995, "\u{79ca}"), (0xF996, "\u{7df4}"),
    (0xF997, "\u{806f}"), (0xF998, "\u{8f26}"), (0xF999, "\u{84ee}"), (0xF99A, "\u{9023}"),
    (0xF99B, "\u{934a}"), (0xF99C, "\u{5217}"), (0xF99D, "\u{52a3}"), (0xF99E, "\u{54bd}"),
    (0xF99F, "\u{70c8}"), (0xF9A0, "\u{88c2}"), (0xF9A1, "\u{8aaa}"), (0xF9A2, "\u{5ec9}"),
    (0xF9A3, "\u{5ff5}"), (0xF9A4, "\u{637b}"), (0xF9A5, "\u{6bae}"), (0xF9A6, "\u{7c3e}"),
    (0xF9A7, "\u{7375}"), (0xF9A8, "\u{4ee4}"), (0xF9A9, "\u{56f9}"), (0xF9AA, "\u{5be7}"),
    (0xF9AB, "\u{5dba}"), (0xF9AC, "\u{601c}"), (0xF9AD, "\u{73b2}"), (0xF9AE, "\u{7469}"),
    (0xF9AF, "\u{7f9a}"), (0xF9B0, "\u{8046}"), (0xF9B1, "\u{9234}"), (0xF9B2, "\u{96f6}"),
    (0xF9B3, "\u{9748}"), (0xF9B4, "\u{9818}"), (0xF9B5, "\u{4f8b}"), (0xF9B6, "\u{79ae}"),
    (0xF9B7, "\u{91b4}"), (0xF9B8, "\u{96b8}"), (0xF9B9, "\u{60e1}"), (0xF9BA, "\u{4e86}"),
    (0xF9BB, "\u{50da}"), (0xF9BC, "\u{5bee}"), (0xF9BD, "\u{5c3f}"), (0xF9BE, "\u{6599}"),
    (0xF9BF, "\u{6a02}"), (0xF9C0, "\u{71ce}"), (0xF9C1, "\u{7642}"), (0xF9C2, "\u{84fc}"),
    (0xF9C3, "\u{907c}"), (0xF9C4, "\u{9f8d}"), (0xF9C5, "\u{6688}"), (0xF9C6, "\u{962e}"),
    (0xF9C7, "\u{5289}"), (0xF9C8, "\u{677b}"), (0xF9C9, "\u{67f3}"), (0xF9CA, "\u{6d41}"),
    (0xF9CB, "\u{6e9c}"), (0xF9CC, "\u{7409}"), (0xF9CD, "\u{7559}"), (0xF9CE, "\u{786b}"),
    (0xF9CF, "\u{7d10}"), (0xF9D0, "\u{985e}"), (0xF9D1, "\u{516d}"), (0xF9D2, "\u{622e}"),
    (0xF9D3, "\u{9678}"), (0xF9D4, "\u{502b}"), (0xF9D5, "\u{5d19}"), (0xF9D6, "\u{6dea}"),
    (0xF9D7, "\u{8f2a}"), (0xF9D8, "\u{5f8b}"), (0xF9D9, "\u{6144}"), (0xF9DA, "\u{6817}"),
    (0xF9DB, "\u{7387}"), (0xF9DC, "\u{9686}"), (0xF9DD, "\u{5229}"), (0xF9DE, "\u{540f}"),
    (0xF9DF, "\u{5c65}"), (0xF9E0, "\u{6613}"), (0xF9E1, "\u{674e}"), (0xF9E2, "\u{68a8}"),
    (0xF9E3, "\u{6ce5}"), (0xF9E4, "\u{7406}"), (0xF9E5, "\u{75e2}"), (0xF9E6, "\u{7f79}"),
    (0xF9E7, "\u{88cf}"), (0xF9E8, "\u{88e1}"), (0xF9E9, "\u{91cc}"), (0xF9EA, "\u{96e2}"),
    (0xF9EB, "\u{533f}"), (0xF9EC, "\u{6eba}"), (0xF9ED, "\u{541d}"), (0xF9EE, "\u{71d0}"),
    (0xF9EF, "\u{7498}"), (0xF9F0, "\u{85fa}"), (0xF9F1, "\u{96a3}"), (0xF9F2, "\u{9c57}"),
    (0xF9F3, "\u{9e9f}"), (0xF9F4, "\u{6797}"), (0xF9F5, "\u{6dcb}"), (0xF9F6, "\u{81e8}"),
    (0xF9F7, "\u{7acb}"), (0xF9F8, "\u{7b20}"), (0xF9F9, "\u{7c92}"), (0xF9FA, "\u{72c0}"),
    (0xF9FB, "\u{7099}"), (0xF9FC, "\u{8b58}"), (0xF9FD, "\u{4ec0}"), (0xF9FE, "\u{8336}"),
    (0xF9FF, "\u{523a}"), (0xFA00, "\u{5207}"), (0xFA01, "\u{5ea6}"), (0xFA02, "\u{62d3}"),
    (0xFA03, "\u{7cd6}"), (0xFA04, "\u{5b85}"), (0xFA05, "\u{6d1e}"), (0xFA06, "\u{66b4}"),
    (0xFA07, "\u{8f3b}"), (0xFA08, "\u{884c}"), (0xFA09, "\u{964d}"), (0xFA0A, "\u{898b}"),
    (0xFA0B, "\u{5ed3}"), (0xFA0C, "\u{5140}"), (0xFA0D, "\u{55c0}"), (0xFA10, "\u{585a}"),
    (0xFA12, "\u{6674}"), (0xFA15, "\u{51de}"), (0xFA16, "\u{732a}"), (0xFA17, "\u{76ca}"),
    (0xFA18, "\u{793c}"), (0xFA19, "\u{795e}"), (0xFA1A, "\u{7965}"), (0xFA1B, "\u{798f}"),
    (0xFA1C, "\u{9756}"), (0xFA1D, "\u{7cbe}"), (0xFA1E, "\u{7fbd}"), (0xFA20, "\u{8612}"),
    (0xFA22, "\u{8af8}"), (0xFA25, "\u{9038}"), (0xFA26, "\u{90fd}"), (0xFA2A, "\u{98ef}"),
    (0xFA2B, "\u{98fc}"), (0xFA2C, "\u{9928}"), (0xFA2D, "\u{9db4}"), (0xFA2E, "\u{90de}"),
    (0xFA2F, "\u{96b7}"), (0xFA30, "\u{4fae}"), (0xFA31, "\u{50e7}"), (0xFA32, "\u{514d}"),
    (0xFA33, "\u{52c9}"), (0xFA34, "\u{52e4}"), (0xFA35, "\u{5351}"), (0xFA36, "\u{559d}"),
    (0xFA37, "\u{5606}"), (0xFA38, "\u{5668}"), (0xFA39, "\u{5840}"), (0xFA3A, "\u{58a8}"),
    (0xFA3B, "\u{5c64}"), (0xFA3C, "\u{5c6e}"), (0xFA3D, "\u{6094}"), (0xFA3E, "\u{6168}"),
    (0xFA3F, "\u{618e}"), (0xFA40, "\u{61f2}"), (0xFA41, "\u{654f}"), (0xFA42, "\u{65e2}"),
    (0xFA43, "\u{6691}"), (0xFA44, "\u{6885}"), (0xFA45, "\u{6d77}"), (0xFA46, "\u{6e1a}"),
    (0xFA47, "\u{6f22}"), (0xFA48, "\u{716e}"), (0xFA49, "\u{722b}"), (0xFA4A, "\u{7422}"),
    (0xFA4B, "\u{7891}"), (0xFA4C, "\u{793e}"), (0xFA4D, "\u{7949}"), (0xFA4E, "\u{7948}"),
    (0xFA4F, "\u{7950}"), (0xFA50, "\u{7956}"), (0xFA51, "\u{795d}"), (0xFA52, "\u{798d}"),
    (0xFA53, "\u{798e}"), (0xFA54, "\u{7a40}"), (0xFA55, "\u{7a81}"), (0xFA56, "\u{7bc0}"),
    (0xFA57, "\u{7df4}"), (0xFA58, "\u{7e09}"), (0xFA59, "\u{7e41}"), (0xFA5A, "\u{7f72}"),
    (0xFA5B, "\u{8005}"), (0xFA5C, "\u{81ed}"), (0xFA5D, "\u{8279}"), (0xFA5E, "\u{8279}"),
    (0xFA5F, "\u{8457}"), (0xFA60, "\u{8910}"), (0xFA61, "\u{8996}"), (0xFA62, "\u{8b01}"),
    (0xFA63, "\u{8b39}"), (0xFA64, "\u{8cd3}"), (0xFA65, "\u{8d08}"), (0xFA66, "\u{8fb6}"),
    (0xFA67, "\u{9038}"), (0xFA68, "\u{96e3}"), (0xFA69, "\u{97ff}"), (0xFA6A, "\u{983b}"),
    (0xFA6B, "\u{6075}"), (0xFA6C, "\u{242ee}"), (0xFA6D, "\u{8218}"), (0xFA70, "\u{4e26}"),
    (0xFA71, "\u{51b5}"), (0xFA72, "\u{5168}"), (0xFA73, "\u{4f80}"), (0xFA74, "\u{5145}"),
    (0xFA75, "\u{5180}"), (0xFA76, "\u{52c7}"), (0xFA77, "\u{52fa}"), (0xFA78, "\u{559d}"),
    (0xFA79, "\u{5555}"), (0xFA7A, "\u{5599}"), (0xFA7B, "\u{55e2}"), (0xFA7C, "\u{585a}"),
    (0xFA7D, "\u{58b3}"), (0xFA7E, "\u{5944}"), (0xFA7F, "\u{5954}"), (0xFA80, "\u{5a62}"),
    (0xFA81, "\u{5b28}"), (0xFA82, "\u{5ed2}"), (0xFA83, "\u{5ed9}"), (0xFA84, "\u{5f69}"),
    (0xFA85, "\u{5fad}"), (0xFA86, "\u{60d8}"), (0xFA87, "\u{614e}"), (0xFA88, "\u{6108}"),
    (0xFA89, "\u{618e}"), (0xFA8A, "\u{6160}"), (0xFA8B, "\u{61f2}"), (0xFA8C, "\u{6234}"),
    (0xFA8D, "\u{63c4}"), (0xFA8E, "\u{641c}"), (0xFA8F, "\u{6452}"), (0xFA90, "\u{6556}"),
    (0xFA91, "\u{6674}"), (0xFA92, "\u{6717}"), (0xFA93, "\u{671b}"), (0xFA94, "\u{6756}"),
    (0xFA95, "\u{6b79}"), (0xFA96, "\u{6bba}"), (0xFA97, "\u{6d41}"), (0xFA98, "\u{6edb}"),
    (0xFA99, "\u{6ecb}"), (0xFA9A, "\u{6f22}"), (0xFA9B, "\u{701e}"), (0xFA9C, "\u{716e}"),
    (0xFA9D, "\u{77a7}"), (0xFA9E, "\u{7235}"), (0xFA9F, "\u{72af}"), (0xFAA0, "\u{732a}"),
    (0xFAA1, "\u{7471}"), (0xFAA2, "\u{7506}"), (0xFAA3, "\u{753b}"), (0xFAA4, "\u{761d}"),
    (0xFAA5, "\u{761f}"), (0xFAA6, "\u{76ca}"), (0xFAA7, "\u{76db}"), (0xFAA8, "\u{76f4}"),
    (0xFAA9, "\u{774a}"), (0xFAAA, "\u{7740}"), (0xFAAB, "\u{78cc}"), (0xFAAC, "\u{7ab1}"),
    (0xFAAD, "\u{7bc0}"), (0xFAAE, "\u{7c7b}"), (0xFAAF, "\u{7d5b}"), (0xFAB0, "\u{7df4}"),
    (0xFAB1, "\u{7f3e}"), (0xFAB2, "\u{8005}"), (0xFAB3, "\u{8352}"), (0xFAB4, "\u{83ef}"),
    (0xFAB5, "\u{8779}"), (0xFAB6, "\u{8941}"), (0xFAB7, "\u{8986}"), (0xFAB8, "\u{8996}"),
    (0xFAB9, "\u{8abf}"), (0xFABA, "\u{8af8}"), (0xFABB, "\u{8acb}"), (0xFABC, "\u{8b01}"),
    (0xFABD, "\u{8afe}"), (0xFABE, "\u{8aed}"), (0xFABF, "\u{8b39}"), (0xFAC0, "\u{8b8a}"),
    (0xFAC1, "\u{8d08}"), (0xFAC2, "\u{8f38}"), (0xFAC3, "\u{9072}"), (0xFAC4, "\u{9199}"),
    (0xFAC5, "\u{9276}"), (0xFAC6, "\u{967c}"), (0xFAC7, "\u{96e3}"), (0xFAC8, "\u{9756}"),
    (0xFAC9, "\u{97db}"), (0xFACA, "\u{97ff}"), (0xFACB, "\u{980b}"), (0xFACC, "\u{983b}"),
    (0xFACD, "\u{9b12}"), (0xFACE, "\u{9f9c}"), (0xFACF, "\u{2284a}"), (0xFAD0, "\u{22844}"),
    (0xFAD1, "\u{233d5}"), (0xFAD2, "\u{3b9d}"), (0xFAD3, "\u{4018}"), (0xFAD4, "\u{4039}"),
    (0xFAD5, "\u{25249}"), (0xFAD6, "\u{25cd0}"), (0xFAD7, "\u{27ed3}"), (0xFAD8, "\u{9f43}"),
    (0xFAD9, "\u{9f8e}"), (0xFB00, "ff"), (0xFB01, "fi"), (0xFB02, "fl"),
    (0xFB03, "ffi"), (0xFB04, "ffl"), (0xFB05, "st"), (0xFB06, "st"),
    (0xFB13, "\u{574}\u{576}"), (0xFB14, "\u{574}\u{565}"), (0xFB15, "\u{574}\u{56b}"), (0xFB16, "\u{57e}\u{576}"),
    (0xFB17, "\u{574}\u{56d}"), (0xFB1D, "\u{5d9}\u{5b4}"), (0xFB1F, "\u{5f2}\u{5b7}"), (0xFB20, "\u{5e2}"),
    (0xFB21, "\u{5d0}"), (0xFB22, "\u{5d3}"), (0xFB23, "\u{5d4}"), (0xFB24, "\u{5db}"),
    (0xFB25, "\u{5dc}"), (0xFB26, "\u{5dd}"), (0xFB27, "\u{5e8}"), (0xFB28, "\u{5ea}"),
    (0xFB29, "+"), (0xFB2A, "\u{5e9}\u{5c1}"), (0xFB2B, "\u{5e9}\u{5c2}"), (0xFB2C, "\u{5e9}\u{5bc}\u{5c1}"),
    (0xFB2D, "\u{5e9}\u{5bc}\u{5c2}"), (0xFB2E, "\u{5d0}\u{5b7}"), (0xFB2F, "\u{5d0}\u{5b8}"), (0xFB30, "\u{5d0}\u{5bc}"),
    (0xFB31, "\u{5d1}\u{5bc}"), (0xFB32, "\u{5d2}\u{5bc}"), (0xFB33, "\u{5d3}\u{5bc}"), (0xFB34, "\u{5d4}\u{5bc}"),
    (0xFB35, "\u{5d5}\u{5bc}"), (0xFB36, "\u{5d6}\u{5bc}"), (0xFB38, "\u{5d8}\u{5bc}"), (0xFB39, "\u{5d9}\u{5bc}"),
    (0xFB3A, "\u{5da}\u{5bc}"), (0xFB3B, "\u{5db}\u{5bc}"), (0xFB3C, "\u{5dc}\u{5bc}"), (0xFB3E, "\u{5de}\u{5bc}"),
    (0xFB40, "\u{5e0}\u{5bc}"), (0xFB41, "\u{5e1}\u{5bc}"), (0xFB43, "\u{5e3}\u{5bc}"), (0xFB44, "\u{5e4}\u{5bc}"),
    (0xFB46, "\u{5e6}\u{5bc}"), (0xFB47, "\u{5e7}\u{5bc}"), (0xFB48, "\u{5e8}\u{5bc}"), (0xFB49, "\u{5e9}\u{5bc}"),
    (0xFB4A, "\u{5ea}\u{5bc}"), (0xFB4B, "\u{5d5}\u{5b9}"), (0xFB4C, "\u{5d1}\u{5bf}"), (0xFB4D, "\u{5db}\u{5bf}"),
    (0xFB4E, "\u{5e4}\u{5bf}"), (0xFB4F, "\u{5d0}\u{5dc}"), (0xFB50, "\u{671}"), (0xFB51, "\u{671}"),
    (0xFB52, "\u{67b}"), (0xFB53, "\u{67b}"), (0xFB54, "\u{67b}"), (0xFB55, "\u{67b}"),
    (0xFB56, "\u{67e}"), (0xFB57, "\u{67e}"), (0xFB58, "\u{67e}"), (0xFB59, "\u{67e}"),
    (0xFB5A, "\u{680}"), (0xFB5B, "\u{680}"), (0xFB5C, "\u{680}"), (0xFB5D, "\u{680}"),
    (0xFB5E, "\u{67a}"), (0xFB5F, "\u{67a}"), (0xFB60, "\u{67a}"), (0xFB61, "\u{67a}"),
    (0xFB62, "\u{67f}"), (0xFB63, "\u{67f}"), (0xFB64, "\u{67f}"), (0xFB65, "\u{67f}"),
    (0xFB66, "\u{679}"), (0xFB67, "\u{679}"), (0xFB68, "\u{679}"), (0xFB69, "\u{679}"),
    (0xFB6A, "\u{6a4}"), (0xFB6B, "\u{6a4}"), (0xFB6C, "\u{6a4}"), (0xFB6D, "\u{6a4}"),
    (0xFB6E, "\u{6a6}"), (0xFB6F, "\u{6a6}"), (0xFB70, "\u{6a6}"), (0xFB71, "\u{6a6}"),
    (0xFB72, "\u{684}"), (0xFB73, "\u{684}"), (0xFB74, "\u{684}"), (0xFB75, "\u{684}"),
    (0xFB76, "\u{683}"), (0xFB77, "\u{683}"), (0xFB78, "\u{683}"), (0xFB79, "\u{683}"),
    (0xFB7A, "\u{686}"), (0xFB7B, "\u{686}"), (0xFB7C, "\u{686}"), (0xFB7D, "\u{686}"),
    (0xFB7E, "\u{687}"), (0xFB7F, "\u{687}"), (0xFB80, "\u{687}"), (0xFB81, "\u{687}"),
    (0xFB82, "\u{68d}"), (0xFB83, "\u{68d}"), (0xFB84, "\u{68c}"), (0xFB85, "\u{68c}"),
    (0xFB86, "\u{68e}"), (0xFB87, "\u{68e}"), (0xFB88, "\u{688}"), (0xFB89, "\u{688}"),
    (0xFB8A, "\u{698}"), (0xFB8B, "\u{698}"), (0xFB8C, "\u{691}"), (0xFB8D, "\u{691}"),
    (0xFB8E, "\u{6a9}"), (0xFB8F, "\u{6a9}"), (0xFB90, "\u{6a9}"), (0xFB91, "\u{6a9}"),
    (0xFB92, "\u{6af}"), (0xFB93, "\u{6af}"), (0xFB94, "\u{6af}"), (0xFB95, "\u{6af}"),
    (0xFB96, "\u{6b3}"), (0xFB97, "\u{6b3}"), (0xFB98, "\u{6b3}"), (0xFB99, "\u{6b3}"),
    (0xFB9A, "\u{6b1}"), (0xFB9B, "\u{6b1}"), (0xFB9C, "\u{6b1}"), (0xFB9D, "\u{6b1}"),
    (0xFB9E, "\u{6ba}"), (0xFB9F, "\u{6ba}"), (0xFBA0, "\u{6bb}"), (0xFBA1, "\u{6bb}"),
    (0xFBA2, "\u{6bb}"), (0xFBA3, "\u{6bb}"), (0xFBA4, "\u{6d5}\u{654}"), (0xFBA5, "\u{6d5}\u{654}"),
    (0xFBA6, "\u{6c1}"), (0xFBA7, "\u{6c1}"), (0xFBA8, "\u{6c1}"), (0xFBA9, "\u{6c1}"),
    (0xFBAA, "\u{6be}"), (0xFBAB, "\u{6be}"), (0xFBAC, "\u{6be}"), (0xFBAD, "\u{6be}"),
    (0xFBAE, "\u{6d2}"), (0xFBAF, "\u{6d2}"), (0xFBB0, "\u{6d2}\u{654}"), (0xFBB1, "\u{6d2}\u{654}"),
    (0xFBD3, "\u{6ad}"), (0xFBD4, "\u{6ad}"), (0xFBD5, "\u{6ad}"), (0xFBD6, "\u{6ad}"),
    (0xFBD7, "\u{6c7}"), (0xFBD8, "\u{6c7}"), (0xFBD9, "\u{6c6}"), (0xFBDA, "\u{6c6}"),
    (0xFBDB, "\u{6c8}"), (0xFBDC, "\u{6c8}"), (0xFBDD, "\u{6c7}\u{674}"), (0xFBDE, "\u{6cb}"),
    (0xFBDF, "\u{6cb}"), (0xFBE0, "\u{6c5}"), (0xFBE1, "\u{6c5}"), (0xFBE2, "\u{6c9}"),
    (0xFBE3, "\u{6c9}"), (0xFBE4, "\u{6d0}"), (0xFBE5, "\u{6d0}"), (0xFBE6, "\u{6d0}"),
    (0xFBE7, "\u{6d0}"), (0xFBE8, "\u{649}"), (0xFBE9, "\u{649}"), (0xFBEA, "\u{64a}\u{654}\u{627}"),
    (0xFBEB, "\u{64a}\u{654}\u{627}"), (0xFBEC, "\u{64a}\u{654}\u{6d5}"), (0xFBED, "\u{64a}\u{654}\u{6d5}"), (0xFBEE, "\u{64a}\u{654}\u{648}"),
    (0xFBEF, "\u{64a}\u{654}\u{648}"), (0xFBF0, "\u{64a}\u{654}\u{6c7}"), (0xFBF1, "\u{64a}\u{654}\u{6c7}"), (0xFBF2, "\u{64a}\u{654}\u{6c6}"),
    (0xFBF3, "\u{64a}\u{654}\u{6c6}"), (0xFBF4, "\u{64a}\u{654}\u{6c8}"), (0xFBF5, "\u{64a}\u{654}\u{6c8}"), (0xFBF6, "\u{64a}\u{654}\u{6d0}"),
    (0xFBF7, "\u{64a}\u{654}\u{6d0}"), (0xFBF8, "\u{64a}\u{654}\u{6d0}"), (0xFBF9, "\u{64a}\u{654}\u{649}"), (0xFBFA, "\u{64a}\u{654}\u{649}"),
    (0xFBFB, "\u{64a}\u{654}\u{649}"), (0xFBFC, "\u{6cc}"), (0xFBFD, "\u{6cc}"), (0xFBFE, "\u{6cc}"),
    (0xFBFF, "\u{6cc}"), (0xFC00, "\u{64a}\u{654}\u{62c}"), (0xFC01, "\u{64a}\u{654}\u{62d}"), (0xFC02, "\u{64a}\u{654}\u{645}"),
    (0xFC03, "\u{64a}\u{654}\u{649}"), (0xFC04, "\u{64a}\u{654}\u{64a}"), (0xFC05, "\u{628}\u{62c}"), (0xFC06, "\u{628}\u{62d}"),
    (0xFC07, "\u{628}\u{62e}"), (0xFC08, "\u{628}\u{645}"), (0xFC09, "\u{628}\u{649}"), (0xFC0A, "\u{628}\u{64a}"),
    (0xFC0B, "\u{62a}\u{62c}"), (0xFC0C, "\u{62a}\u{62d}"), (0xFC0D, "\u{62a}\u{62e}"), (0xFC0E, "\u{62a}\u{645}"),
    (0xFC0F, "\u{62a}\u{649}"), (0xFC10, "\u{62a}\u{64a}"), (0xFC11, "\u{62b}\u{62c}"), (0xFC12, "\u{62b}\u{645}"),
    (0xFC13, "\u{62b}\u{649}"), (0xFC14, "\u{62b}\u{64a}"), (0xFC15, "\u{62c}\u{62d}"), (0xFC16, "\u{62c}\u{645}"),
    (0xFC17, "\u{62d}\u{62c}"), (0xFC18, "\u{62d}\u{645}"), (0xFC19, "\u{62e}\u{62c}"), (0xFC1A, "\u{62e}\u{62d}"),
    (0xFC1B, "\u{62e}\u{645}"), (0xFC1C, "\u{633}\u{62c}"), (0xFC1D, "\u{633}\u{62d}"), (0xFC1E, "\u{633}\u{62e}"),
    (0xFC1F, "\u{633}\u{645}"), (0xFC20, "\u{635}\u{62d}"), (0xFC21, "\u{635}\u{645}"), (0xFC22, "\u{636}\u{62c}"),
    (0xFC23, "\u{636}\u{62d}"), (0xFC24, "\u{636}\u{62e}"), (0xFC25, "\u{636}\u{645}"), (0xFC26, "\u{637}\u{62d}"),
    (0xFC27, "\u{637}\u{645}"), (0xFC28, "\u{638}\u{645}"), (0xFC29, "\u{639}\u{62c}"), (0xFC2A, "\u{639}\u{645}"),
    (0xFC2B, "\u{63a}\u{62c}"), (0xFC2C, "\u{63a}\u{645}"), (0xFC2D, "\u{641}\u{62c}"), (0xFC2E, "\u{641}\u{62d}"),
    (0xFC2F, "\u{641}\u{62e}"), (0xFC30, "\u{641}\u{645}"), (0xFC31, "\u{641}\u{649}"), (0xFC32, "\u{641}\u{64a}"),
    (0xFC33, "\u{642}\u{62d}"), (0xFC34, "\u{642}\u{645}"), (0xFC35, "\u{642}\u{649}"), (0xFC36, "\u{642}\u{64a}"),
    (0xFC37, "\u{643}\u{627}"), (0xFC38, "\u{643}\u{62c}"), (0xFC39, "\u{643}\u{62d}"), (0xFC3A, "\u{643}\u{62e}"),
    (0xFC3B, "\u{643}\u{644}"), (0xFC3C, "\u{643}\u{645}"), (0xFC3D, "\u{643}\u{649}"), (0xFC3E, "\u{643}\u{64a}"),
    (0xFC3F, "\u{644}\u{62c}"), (0xFC40, "\u{644}\u{62d}"), (0xFC41, "\u{644}\u{62e}"), (0xFC42, "\u{644}\u{645}"),
    (0xFC43, "\u{644}\u{649}"), (0xFC44, "\u{644}\u{64a}"), (0xFC45, "\u{645}\u{62c}"), (0xFC46, "\u{645}\u{62d}"),
    (0xFC47, "\u{645}\u{62e}"), (0xFC48, "\u{645}\u{645}"), (0xFC49, "\u{645}\u{649}"), (0xFC4A, "\u{645}\u{64a}"),
    (0xFC4B, "\u{646}\u{62c}"), (0xFC4C, "\u{646}\u{62d}"), (0xFC4D, "\u{646}\u{62e}"), (0xFC4E, "\u{646}\u{645}"),
    (0xFC4F, "\u{646}\u{649}"), (0xFC50, "\u{646}\u{64a}"), (0xFC51, "\u{647}\u{62c}"), (0xFC52, "\u{647}\u{645}"),
    (0xFC53, "\u{647}\u{649}"), (0xFC54, "\u{647}\u{64a}"), (0xFC55, "\u{64a}\u{62c}"), (0xFC56, "\u{64a}\u{62d}"),
    (0xFC57, "\u{64a}\u{62e}"), (0xFC58, "\u{64a}\u{645}"), (0xFC59, "\u{64a}\u{649}"), (0xFC5A, "\u{64a}\u{64a}"),
    (0xFC5B, "\u{630}\u{670}"), (0xFC5C, "\u{631}\u{670}"), (0xFC5D, "\u{649}\u{670}"), (0xFC5E, " \u{64c}\u{651}"),
    (0xFC5F, " \u{64d}\u{651}"), (0xFC60, " \u{64e}\u{651}"), (0xFC61, " \u{64f}\u{651}"), (0xFC62, " \u{650}\u{651}"),
    (0xFC63, " \u{651}\u{670}"), (0xFC64, "\u{64a}\u{654}\u{631}"), (0xFC65, "\u{64a}\u{654}\u{632}"), (0xFC66, "\u{64a}\u{654}\u{645}"),
    (0xFC67, "\u{64a}\u{654}\u{646}"), (0xFC68, "\u{64a}\u{654}\u{649}"), (0xFC69, "\u{64a}\u{654}\u{64a}"), (0xFC6A, "\u{628}\u{631}"),
    (0xFC6B, "\u{628}\u{632}"), (0xFC6C, "\u{628}\u{645}"), (0xFC6D, "\u{628}\u{646}"), (0xFC6E, "\u{628}\u{649}"),
    (0xFC6F, "\u{628}\u{64a}"), (0xFC70, "\u{62a}\u{631}"), (0xFC71, "\u{62a}\u{632}"), (0xFC72, "\u{62a}\u{645}"),
    (0xFC73, "\u{62a}\u{646}"), (0xFC74, "\u{62a}\u{649}"), (0xFC75, "\u{62a}\u{64a}"), (0xFC76, "\u{62b}\u{631}"),
    (0xFC77, "\u{62b}\u{632}"), (0xFC78, "\u{62b}\u{645}"), (0xFC79, "\u{62b}\u{646}"), (0xFC7A, "\u{62b}\u{649}"),
    (0xFC7B, "\u{62b}\u{64a}"), (0xFC7C, "\u{641}\u{649}"), (0xFC7D, "\u{641}\u{64a}"), (0xFC7E, "\u{642}\u{649}"),
    (0xFC7F, "\u{642}\u{64a}"), (0xFC80, "\u{643}\u{627}"), (0xFC81, "\u{643}\u{644}"), (0xFC82, "\u{643}\u{645}"),
    (0xFC83, "\u{643}\u{649}"), (0xFC84, "\u{643}\u{64a}"), (0xFC85, "\u{644}\u{645}"), (0xFC86, "\u{644}\u{649}"),
    (0xFC87, "\u{644}\u{64a}"), (0xFC88, "\u{645}\u{627}"), (0xFC89, "\u{645}\u{645}"), (0xFC8A, "\u{646}\u{631}"),
    (0xFC8B, "\u{646}\u{632}"), (0xFC8C, "\u{646}\u{645}"), (0xFC8D, "\u{646}\u{646}"), (0xFC8E, "\u{646}\u{649}"),
    (0xFC8F, "\u{646}\u{64a}"), (0xFC90, "\u{649}\u{670}"), (0xFC91, "\u{64a}\u{631}"), (0xFC92, "\u{64a}\u{632}"),
    (0xFC93, "\u{64a}\u{645}"), (0xFC94, "\u{64a}\u{646}"), (0xFC95, "\u{64a}\u{649}"), (0xFC96, "\u{64a}\u{64a}"),
    (0xFC97, "\u{64a}\u{654}\u{62c}"), (0xFC98, "\u{64a}\u{654}\u{62d}"), (0xFC99, "\u{64a}\u{654}\u{62e}"), (0xFC9A, "\u{64a}\u{654}\u{645}"),
    (0xFC9B, "\u{64a}\u{654}\u{647}"), (0xFC9C, "\u{628}\u{62c}"), (0xFC9D, "\u{628}\u{62d}"), (0xFC9E, "\u{628}\u{62e}"),
    (0xFC9F, "\u{628}\u{645}"), (0xFCA0, "\u{628}\u{647}"), (0xFCA1, "\u{62a}\u{62c}"), (0xFCA2, "\u{62a}\u{62d}"),
    (0xFCA3, "\u{62a}\u{62e}"), (0xFCA4, "\u{62a}\u{645}"), (0xFCA5, "\u{62a}\u{647}"), (0xFCA6, "\u{62b}\u{645}"),
    (0xFCA7, "\u{62c}\u{62d}"), (0xFCA8, "\u{62c}\u{645}"), (0xFCA9, "\u{62d}\u{62c}"), (0xFCAA, "\u{62d}\u{645}"),
    (0xFCAB, "\u{62e}\u{62c}"), (0xFCAC, "\u{62e}\u{645}"), (0xFCAD, "\u{633}\u{62c}"), (0xFCAE, "\u{633}\u{62d}"),
    (0xFCAF, "\u{633}\u{62e}"), (0xFCB0, "\u{633}\u{645}"), (0xFCB1, "\u{635}\u{62d}"), (0xFCB2, "\u{635}\u{62e}"),
    (0xFCB3, "\u{635}\u{645}"), (0xFCB4, "\u{636}\u{62c}"), (0xFCB5, "\u{636}\u{62d}"), (0xFCB6, "\u{636}\u{62e}"),
    (0xFCB7, "\u{636}\u{645}"), (0xFCB8, "\u{637}\u{62d}"), (0xFCB9, "\u{638}\u{645}"), (0xFCBA, "\u{639}\u{62c}"),
    (0xFCBB, "\u{639}\u{645}"), (0xFCBC, "\u{63a}\u{62c}"), (0xFCBD, "\u{63a}\u{645}"), (0xFCBE, "\u{641}\u{62c}"),
    (0xFCBF, "\u{641}\u{62d}"), (0xFCC0, "\u{641}\u{62e}"), (0xFCC1, "\u{641}\u{645}"), (0xFCC2, "\u{642}\u{62d}"),
    (0xFCC3, "\u{642}\u{645}"), (0xFCC4, "\u{643}\u{62c}"), (0xFCC5, "\u{643}\u{62d}"), (0xFCC6, "\u{643}\u{62e}"),
    (0xFCC7, "\u{643}\u{644}"), (0xFCC8, "\u{643}\u{645}"), (0xFCC9, "\u{644}\u{62c}"), (0xFCCA, "\u{644}\u{62d}"),
    (0xFCCB, "\u{644}\u{62e}"), (0xFCCC, "\u{644}\u{645}"), (0xFCCD, "\u{644}\u{647}"), (0xFCCE, "\u{645}\u{62c}"),
    (0xFCCF, "\u{645}\u{62d}"), (0xFCD0, "\u{645}\u{62e}"), (0xFCD1, "\u{645}\u{645}"), (0xFCD2, "\u{646}\u{62c}"),
    (0xFCD3, "\u{646}\u{62d}"), (0xFCD4, "\u{646}\u{62e}"), (0xFCD5, "\u{646}\u{645}"), (0xFCD6, "\u{646}\u{647}"),
    (0xFCD7, "\u{647}\u{62c}"), (0xFCD8, "\u{647}\u{645}"), (0xFCD9, "\u{647}\u{670}"), (0xFCDA, "\u{64a}\u{62c}"),
    (0xFCDB, "\u{64a}\u{62d}"), (0xFCDC, "\u{64a}\u{62e}"), (0xFCDD, "\u{64a}\u{645}"), (0xFCDE, "\u{64a}\u{647}"),
    (0xFCDF, "\u{64a}\u{654}\u{645}"), (0xFCE0, "\u{64a}\u{654}\u{647}"), (0xFCE1, "\u{628}\u{645}"), (0xFCE2, "\u{628}\u{647}"),
    (0xFCE3, "\u{62a}\u{645}"), (0xFCE4, "\u{62a}\u{647}"), (0xFCE5, "\u{62b}\u{645}"), (0xFCE6, "\u{62b}\u{647}"),
    (0xFCE7, "\u{633}\u{645}"), (0xFCE8, "\u{633}\u{647}"), (0xFCE9, "\u{634}\u{645}"), (0xFCEA, "\u{634}\u{647}"),
    (0xFCEB, "\u{643}\u{644}"), (0xFCEC, "\u{643}\u{645}"), (0xFCED, "\u{644}\u{645}"), (0xFCEE, "\u{646}\u{645}"),
    (0xFCEF, "\u{646}\u{647}"), (0xFCF0, "\u{64a}\u{645}"), (0xFCF1, "\u{64a}\u{647}"), (0xFCF2, "\u{640}\u{64e}\u{651}"),
    (0xFCF3, "\u{640}\u{64f}\u{651}"), (0xFCF4, "\u{640}\u{650}\u{651}"), (0xFCF5, "\u{637}\u{649}"), (0xFCF6, "\u{637}\u{64a}"),
    (0xFCF7, "\u{639}\u{649}"), (0xFCF8, "\u{639}\u{64a}"), (0xFCF9, "\u{63a}\u{649}"), (0xFCFA, "\u{63a}\u{64a}"),
    (0xFCFB, "\u{633}\u{649}"), (0xFCFC, "\u{633}\u{64a}"), (0xFCFD, "\u{634}\u{649}"), (0xFCFE, "\u{634}\u{64a}"),
    (0xFCFF, "\u{62d}\u{649}"), (0xFD00, "\u{62d}\u{64a}"), (0xFD01, "\u{62c}\u{649}"), (0xFD02, "\u{62c}\u{64a}"),
    (0xFD03, "\u{62e}\u{649}"), (0xFD04, "\u{62e}\u{64a}"), (0xFD05, "\u{635}\u{649}"), (0xFD06, "\u{635}\u{64a}"),
    (0xFD07, "\u{636}\u{649}"), (0xFD08, "\u{636}\u{64a}"), (0xFD09, "\u{634}\u{62c}"), (0xFD0A, "\u{634}\u{62d}"),
    (0xFD0B, "\u{634}\u{62e}"), (0xFD0C, "\u{634}\u{645}"), (0xFD0D, "\u{634}\u{631}"), (0xFD0E, "\u{633}\u{631}"),
    (0xFD0F, "\u{635}\u{631}"), (0xFD10, "\u{636}\u{631}"), (0xFD11, "\u{637}\u{649}"), (0xFD12, "\u{637}\u{64a}"),
    (0xFD13, "\u{639}\u{649}"), (0xFD14, "\u{639}\u{64a}"), (0xFD15, "\u{63a}\u{649}"), (0xFD16, "\u{63a}\u{64a}"),
    (0xFD17, "\u{633}\u{649}"), (0xFD18, "\u{633}\u{64a}"), (0xFD19, "\u{634}\u{649}"), (0xFD1A, "\u{634}\u{64a}"),
    (0xFD1B, "\u{62d}\u{649}"), (0xFD1C, "\u{62d}\u{64a}"), (0xFD1D, "\u{62c}\u{649}"), (0xFD1E, "\u{62c}\u{64a}"),
    (0xFD1F, "\u{62e}\u{649}"), (0xFD20, "\u{62e}\u{64a}"), (0xFD21, "\u{635}\u{649}"), (0xFD22, "\u{635}\u{64a}"),
    (0xFD23, "\u{636}\u{649}"), (0xFD24, "\u{636}\u{64a}"), (0xFD25, "\u{634}\u{62c}"), (0xFD26, "\u{634}\u{62d}"),
    (0xFD27, "\u{634}\u{62e}"), (0xFD28, "\u{634}\u{645}"), (0xFD29, "\u{634}\u{631}"), (0xFD2A, "\u{633}\u{631}"),
    (0xFD2B, "\u{635}\u{631}"), (0xFD2C, "\u{636}\u{631}"), (0xFD2D, "\u{634}\u{62c}"), (0xFD2E, "\u{634}\u{62d}"),
    (0xFD2F, "\u{634}\u{62e}"), (0xFD30, "\u{634}\u{645}"), (0xFD31, "\u{633}\u{647}"), (0xFD32, "\u{634}\u{647}"),
    (0xFD33, "\u{637}\u{645}"), (0xFD34, "\u{633}\u{62c}"), (0xFD35, "\u{633}\u{62d}"), (0xFD36, "\u{633}\u{62e}"),
    (0xFD37, "\u{634}\u{62c}"), (0xFD38, "\u{634}\u{62d}"), (0xFD39, "\u{634}\u{62e}"), (0xFD3A, "\u{637}\u{645}"),
    (0xFD3B, "\u{638}\u{645}"), (0xFD3C, "\u{627}\u{64b}"), (0xFD3D, "\u{627}\u{64b}"), (0xFD50, "\u{62a}\u{62c}\u{645}"),
    (0xFD51, "\u{62a}\u{62d}\u{62c}"), (0xFD52, "\u{62a}\u{62d}\u{62c}"), (0xFD53, "\u{62a}\u{62d}\u{645}"), (0xFD54, "\u{62a}\u{62e}\u{645}"),
    (0xFD55, "\u{62a}\u{645}\u{62c}"), (0xFD56, "\u{62a}\u{645}\u{62d}"), (0xFD57, "\u{62a}\u{645}\u{62e}"), (0xFD58, "\u{62c}\u{645}\u{62d}"),
    (0xFD59, "\u{62c}\u{645}\u{62d}"), (0xFD5A, "\u{62d}\u{645}\u{64a}"), (0xFD5B, "\u{62d}\u{645}\u{649}"), (0xFD5C, "\u{633}\u{62d}\u{62c}"),
    (0xFD5D, "\u{633}\u{62c}\u{62d}"), (0xFD5E, "\u{633}\u{62c}\u{649}"), (0xFD5F, "\u{633}\u{645}\u{62d}"), (0xFD60, "\u{633}\u{645}\u{62d}"),
    (0xFD61, "\u{633}\u{645}\u{62c}"), (0xFD62, "\u{633}\u{645}\u{645}"), (0xFD63, "\u{633}\u{645}\u{645}"), (0xFD64, "\u{635}\u{62d}\u{62d}"),
    (0xFD65, "\u{635}\u{62d}\u{62d}"), (0xFD66, "\u{635}\u{645}\u{645}"), (0xFD67, "\u{634}\u{62d}\u{645}"), (0xFD68, "\u{634}\u{62d}\u{645}"),
    (0xFD69, "\u{634}\u{62c}\u{64a}"), (0xFD6A, "\u{634}\u{645}\u{62e}"), (0xFD6B, "\u{634}\u{645}\u{62e}"), (0xFD6C, "\u{634}\u{645}\u{645}"),
    (0xFD6D, "\u{634}\u{645}\u{645}"), (0xFD6E, "\u{636}\u{62d}\u{649}"), (0xFD6F, "\u{636}\u{62e}\u{645}"), (0xFD70, "\u{636}\u{62e}\u{645}"),
    (0xFD71, "\u{637}\u{645}\u{62d}"), (0xFD72, "\u{637}\u{645}\u{62d}"), (0xFD73, "\u{637}\u{645}\u{645}"), (0xFD74, "\u{637}\u{645}\u{64a}"),
    (0xFD75, "\u{639}\u{62c}\u{645}"), (0xFD76, "\u{639}\u{645}\u{645}"), (0xFD77, "\u{639}\u{645}\u{645}"), (0xFD78, "\u{639}\u{645}\u{649}"),
    (0xFD79, "\u{63a}\u{645}\u{645}"), (0xFD7A, "\u{63a}\u{645}\u{64a}"), (0xFD7B, "\u{63a}\u{645}\u{649}"), (0xFD7C, "\u{641}\u{62e}\u{645}"),
    (0xFD7D, "\u{641}\u{62e}\u{645}"), (0xFD7E, "\u{642}\u{645}\u{62d}"), (0xFD7F, "\u{642}\u{645}\u{645}"), (0xFD80, "\u{644}\u{62d}\u{645}"),
    (0xFD81, "\u{644}\u{62d}\u{64a}"), (0xFD82, "\u{644}\u{62d}\u{649}"), (0xFD83, "\u{644}\u{62c}\u{62c}"), (0xFD84, "\u{644}\u{62c}\u{62c}"),
    (0xFD85, "\u{644}\u{62e}\u{645}"), (0xFD86, "\u{644}\u{62e}\u{645}"), (0xFD87, "\u{644}\u{645}\u{62d}"), (0xFD88, "\u{644}\u{645}\u{62d}"),
    (0xFD89, "\u{645}\u{62d}\u{62c}"), (0xFD8A, "\u{645}\u{62d}\u{645}"), (0xFD8B, "\u{645}\u{62d}\u{64a}"), (0xFD8C, "\u{645}\u{62c}\u{62d}"),
    (0xFD8D, "\u{645}\u{62c}\u{645}"), (0xFD8E, "\u{645}\u{62e}\u{62c}"), (0xFD8F, "\u{645}\u{62e}\u{645}"), (0xFD92, "\u{645}\u{62c}\u{62e}"),
    (0xFD93, "\u{647}\u{645}\u{62c}"), (0xFD94, "\u{647}\u{645}\u{645}"), (0xFD95, "\u{646}\u{62d}\u{645}"), (0xFD96, "\u{646}\u{62d}\u{649}"),
    (0xFD97, "\u{646}\u{62c}\u{645}"), (0xFD98, "\u{646}\u{62c}\u{645}"), (0xFD99, "\u{646}\u{62c}\u{649}"), (0xFD9A, "\u{646}\u{645}\u{64a}"),
    (0xFD9B, "\u{646}\u{645}\u{649}"), (0xFD9C, "\u{64a}\u{645}\u{645}"), (0xFD9D, "\u{64a}\u{645}\u{645}"), (0xFD9E, "\u{628}\u{62e}\u{64a}"),
    (0xFD9F, "\u{62a}\u{62c}\u{64a}"), (0xFDA0, "\u{62a}\u{62c}\u{649}"), (0xFDA1, "\u{62a}\u{62e}\u{64a}"), (0xFDA2, "\u{62a}\u{62e}\u{649}"),
    (0xFDA3, "\u{62a}\u{645}\u{64a}"), (0xFDA4, "\u{62a}\u{645}\u{649}"), (0xFDA5, "\u{62c}\u{645}\u{64a}"), (0xFDA6, "\u{62c}\u{62d}\u{649}"),
    (0xFDA7, "\u{62c}\u{645}\u{649}"), (0xFDA8, "\u{633}\u{62e}\u{649}"), (0xFDA9, "\u{635}\u{62d}\u{64a}"), (0xFDAA, "\u{634}\u{62d}\u{64a}"),
    (0xFDAB, "\u{636}\u{62d}\u{64a}"), (0xFDAC, "\u{644}\u{62c}\u{64a}"), (0xFDAD, "\u{644}\u{645}\u{64a}"), (0xFDAE, "\u{64a}\u{62d}\u{64a}"),
    (0xFDAF, "\u{64a}\u{62c}\u{64a}"), (0xFDB0, "\u{64a}\u{645}\u{64a}"), (0xFDB1, "\u{645}\u{645}\u{64a}"), (0xFDB2, "\u{642}\u{645}\u{64a}"),
    (0xFDB3, "\u{646}\u{62d}\u{64a}"), (0xFDB4, "\u{642}\u{645}\u{62d}"), (0xFDB5, "\u{644}\u{62d}\u{645}"), (0xFDB6, "\u{639}\u{645}\u{64a}"),
    (0xFDB7, "\u{643}\u{645}\u{64a}"), (0xFDB8, "\u{646}\u{62c}\u{62d}"), (0xFDB9, "\u{645}\u{62e}\u{64a}"), (0xFDBA, "\u{644}\u{62c}\u{645}"),
    (0xFDBB, "\u{643}\u{645}\u{645}"), (0xFDBC, "\u{644}\u{62c}\u{645}"), (0xFDBD, "\u{646}\u{62c}\u{62d}"), (0xFDBE, "\u{62c}\u{62d}\u{64a}"),
    (0xFDBF, "\u{62d}\u{62c}\u{64a}"), (0xFDC0, "\u{645}\u{62c}\u{64a}"), (0xFDC1, "\u{641}\u{645}\u{64a}"), (0xFDC2, "\u{628}\u{62d}\u{64a}"),
    (0xFDC3, "\u{643}\u{645}\u{645}"), (0xFDC4, "\u{639}\u{62c}\u{645}"), (0xFDC5, "\u{635}\u{645}\u{645}"), (0xFDC6, "\u{633}\u{62e}\u{64a}"),
    (0xFDC7, "\u{646}\u{62c}\u{64a}"), (0xFDF0, "\u{635}\u{644}\u{6d2}"), (0xFDF1, "\u{642}\u{644}\u{6d2}"), (0xFDF2, "\u{627}\u{644}\u{644}\u{647}"),
    (0xFDF3, "\u{627}\u{643}\u{628}\u{631}"), (0xFDF4, "\u{645}\u{62d}\u{645}\u{62f}"), (0xFDF5, "\u{635}\u{644}\u{639}\u{645}"), (0xFDF6, "\u{631}\u{633}\u{648}\u{644}"),
    (0xFDF7, "\u{639}\u{644}\u{64a}\u{647}"), (0xFDF8, "\u{648}\u{633}\u{644}\u{645}"), (0xFDF9, "\u{635}\u{644}\u{649}"), (0xFDFA, "\u{635}\u{644}\u{649} \u{627}\u{644}\u{644}\u{647} \u{639}\u{644}\u{64a}\u{647} \u{648}\u{633}\u{644}\u{645}"),
    (0xFDFB, "\u{62c}\u{644} \u{62c}\u{644}\u{627}\u{644}\u{647}"), (0xFDFC, "\u{631}\u{6cc}\u{627}\u{644}"), (0xFE10, ","), (0xFE11, "\u{3001}"),
    (0xFE12, "\u{3002}"), (0xFE13, ":"), (0xFE14, ";"), (0xFE15, "!"),
    (0xFE16, "?"), (0xFE17, "\u{3016}"), (0xFE18, "\u{3017}"), (0xFE19, "..."),
    (0xFE30, ".."), (0xFE31, "\u{2014}"), (0xFE32, "\u{2013}"), (0xFE33, "_"),
    (0xFE34, "_"), (0xFE35, "("), (0xFE36, ")"), (0xFE37, "{"),
    (0xFE38, "}"), (0xFE39, "\u{3014}"), (0xFE3A, "\u{3015}"), (0xFE3B, "\u{3010}"),
    (0xFE3C, "\u{3011}"), (0xFE3D, "\u{300a}"), (0xFE3E, "\u{300b}"), (0xFE3F, "\u{3008}"),
    (0xFE40, "\u{3009}"), (0xFE41, "\u{300c}"), (0xFE42, "\u{300d}"), (0xFE43, "\u{300e}"),
    (0xFE44, "\u{300f}"), (0xFE47, "["), (0xFE48, "]"), (0xFE49, " \u{305}"),
    (0xFE4A, " \u{305}"), (0xFE4B, " \u{305}"), (0xFE4C, " \u{305}"), (0xFE4D, "_"),
    (0xFE4E, "_"), (0xFE4F, "_"), (0xFE50, ","), (0xFE51, "\u{3001}"),
    (0xFE52, "."), (0xFE54, ";"), (0xFE55, ":"), (0xFE56, "?"),
    (0xFE57, "!"), (0xFE58, "\u{2014}"), (0xFE59, "("), (0xFE5A, ")"),
    (0xFE5B, "{"), (0xFE5C, "}"), (0xFE5D, "\u{3014}"), (0xFE5E, "\u{3015}"),
    (0xFE5F, "#"), (0xFE60, "&"), (0xFE61, "*"), (0xFE62, "+"),
    (0xFE63, "-"), (0xFE64, "<"), (0xFE65, ">"), (0xFE66, "="),
    (0xFE68, "\\"), (0xFE69, "$"), (0xFE6A, "%"), (0xFE6B, "@"),
    (0xFE70, " \u{64b}"), (0xFE71, "\u{640}\u{64b}"), (0xFE72, " \u{64c}"), (0xFE74, " \u{64d}"),
    (0xFE76, " \u{64e}"), (0xFE77, "\u{640}\u{64e}"), (0xFE78, " \u{64f}"), (0xFE79, "\u{640}\u{64f}"),
    (0xFE7A, " \u{650}"), (0xFE7B, "\u{640}\u{650}"), (0xFE7C, " \u{651}"), (0xFE7D, "\u{640}\u{651}"),
    (0xFE7E, " \u{652}"), (0xFE7F, "\u{640}\u{652}"), (0xFE80, "\u{621}"), (0xFE81, "\u{627}\u{653}"),
    (0xFE82, "\u{627}\u{653}"), (0xFE83, "\u{627}\u{654}"), (0xFE84, "\u{627}\u{654}"), (0xFE85, "\u{648}\u{654}"),
    (0xFE86, "\u{648}\u{654}"), (0xFE87, "\u{627}\u{655}"), (0xFE88, "\u{627}\u{655}"), (0xFE89, "\u{64a}\u{654}"),
    (0xFE8A, "\u{64a}\u{654}"), (0xFE8B, "\u{64a}\u{654}"), (0xFE8C, "\u{64a}\u{654}"), (0xFE8D, "\u{627}"),
    (0xFE8E, "\u{627}"), (0xFE8F, "\u{628}"), (0xFE90, "\u{628}"), (0xFE91, "\u{628}"),
    (0xFE92, "\u{628}"), (0xFE93, "\u{629}"), (0xFE94, "\u{629}"), (0xFE95, "\u{62a}"),
    (0xFE96, "\u{62a}"), (0xFE97, "\u{62a}"), (0xFE98, "\u{62a}"), (0xFE99, "\u{62b}"),
    (0xFE9A, "\u{62b}"), (0xFE9B, "\u{62b}"), (0xFE9C, "\u{62b}"), (0xFE9D, "\u{62c}"),
    (0xFE9E, "\u{62c}"), (0xFE9F, "\u{62c}"), (0xFEA0, "\u{62c}"), (0xFEA1, "\u{62d}"),
    (0xFEA2, "\u{62d}"), (0xFEA3, "\u{62d}"), (0xFEA4, "\u{62d}"), (0xFEA5, "\u{62e}"),
    (0xFEA6, "\u{62e}"), (0xFEA7, "\u{62e}"), (0xFEA8, "\u{62e}"), (0xFEA9, "\u{62f}"),
    (0xFEAA, "\u{62f}"), (0xFEAB, "\u{630}"), (0xFEAC, "\u{630}"), (0xFEAD, "\u{631}"),
    (0xFEAE, "\u{631}"), (0xFEAF, "\u{632}"), (0xFEB0, "\u{632}"), (0xFEB1, "\u{633}"),
    (0xFEB2, "\u{633}"), (0xFEB3, "\u{633}"), (0xFEB4, "\u{633}"), (0xFEB5, "\u{634}"),
    (0xFEB6, "\u{634}"), (0xFEB7, "\u{634}"), (0xFEB8, "\u{634}"), (0xFEB9, "\u{635}"),
    (0xFEBA, "\u{635}"), (0xFEBB, "\u{635}"), (0xFEBC, "\u{635}"), (0xFEBD, "\u{636}"),
    (0xFEBE, "\u{636}"), (0xFEBF, "\u{636}"), (0xFEC0, "\u{636}"), (0xFEC1, "\u{637}"),
    (0xFEC2, "\u{637}"), (0xFEC3, "\u{637}"), (0xFEC4, "\u{637}"), (0xFEC5, "\u{638}"),
    (0xFEC6, "\u{638}"), (0xFEC7, "\u{638}"), (0xFEC8, "\u{638}"), (0xFEC9, "\u{639}"),
    (0xFECA, "\u{639}"), (0xFECB, "\u{639}"), (0xFECC, "\u{639}"), (0xFECD, "\u{63a}"),
    (0xFECE, "\u{63a}"), (0xFECF, "\u{63a}"), (0xFED0, "\u{63a}"), (0xFED1, "\u{641}"),
    (0xFED2, "\u{641}"), (0xFED3, "\u{641}"), (0xFED4, "\u{641}"), (0xFED5, "\u{642}"),
    (0xFED6, "\u{642}"), (0xFED7, "\u{642}"), (0xFED8, "\u{642}"), (0xFED9, "\u{643}"),
    (0xFEDA, "\u{643}"), (0xFEDB, "\u{643}"), (0xFEDC, "\u{643}"), (0xFEDD, "\u{644}"),
    (0xFEDE, "\u{644}"), (0xFEDF, "\u{644}"), (0xFEE0, "\u{644}"), (0xFEE1, "\u{645}"),
    (0xFEE2, "\u{645}"), (0xFEE3, "\u{645}"), (0xFEE4, "\u{645}"), (0xFEE5, "\u{646}"),
    (0xFEE6, "\u{646}"), (0xFEE7, "\u{646}"), (0xFEE8, "\u{646}"), (0xFEE9, "\u{647}"),
    (0xFEEA, "\u{647}"), (0xFEEB, "\u{647}"), (0xFEEC, "\u{647}"), (0xFEED, "\u{648}"),
    (0xFEEE, "\u{648}"), (0xFEEF, "\u{649}"), (0xFEF0, "\u{649}"), (0xFEF1, "\u{64a}"),
    (0xFEF2, "\u{64a}"), (0xFEF3, "\u{64a}"), (0xFEF4, "\u{64a}"), (0xFEF5, "\u{644}\u{627}\u{653}"),
    (0xFEF6, "\u{644}\u{627}\u{653}"), (0xFEF7, "\u{644}\u{627}\u{654}"), (0xFEF8, "\u{644}\u{627}\u{654}"), (0xFEF9, "\u{644}\u{627}\u{655}"),
    (0xFEFA, "\u{644}\u{627}\u{655}"), (0xFEFB, "\u{644}\u{627}"), (0xFEFC, "\u{644}\u{627}"), (0xFF01, "!"),
    (0xFF02, "\""), (0xFF03, "#"), (0xFF04, "$"), (0xFF05, "%"),
    (0xFF06, "&"), (0xFF07, "'"), (0xFF08, "("), (0xFF09, ")"),
    (0xFF0A, "*"), (0xFF0B, "+"), (0xFF0C, ","), (0xFF0D, "-"),
    (0xFF0E, "."), (0xFF0F, "/"), (0xFF10, "0"), (0xFF11, "1"),
    (0xFF12, "2"), (0xFF13, "3"), (0xFF14, "4"), (0xFF15, "5"),
    (0xFF16, "6"), (0xFF17, "7"), (0xFF18, "8"), (0xFF19, "9"),
    (0xFF1A, ":"), (0xFF1B, ";"), (0xFF1C, "<"), (0xFF1D, "="),
    (0xFF1E, ">"), (0xFF1F, "?"), (0xFF20, "@"), (0xFF21, "A"),
    (0xFF22, "B"), (0xFF23, "C"), (0xFF24, "D"), (0xFF25, "E"),
    (0xFF26, "F"), (0xFF27, "G"), (0xFF28, "H"), (0xFF29, "I"),
    (0xFF2A, "J"), (0xFF2B, "K"), (0xFF2C, "L"), (0xFF2D, "M"),
    (0xFF2E, "N"), (0xFF2F, "O"), (0xFF30, "P"), (0xFF31, "Q"),
    (0xFF32, "R"), (0xFF33, "S"), (0xFF34, "T"), (0xFF35, "U"),
    (0xFF36, "V"), (0xFF37, "W"), (0xFF38, "X"), (0xFF39, "Y"),
    (0xFF3A, "Z"), (0xFF3B, "["), (0xFF3C, "\\"), (0xFF3D, "]"),
    (0xFF3E, "^"), (0xFF3F, "_"), (0xFF40, "`"), (0xFF41, "a"),
    (0xFF42, "b"), (0xFF43, "c"), (0xFF44, "d"), (0xFF45, "e"),
    (0xFF46, "f"), (0xFF47, "g"), (0xFF48, "h"), (0xFF49, "i"),
    (0xFF4A, "j"), (0xFF4B, "k"), (0xFF4C, "l"), (0xFF4D, "m"),
    (0xFF4E, "n"), (0xFF4F, "o"), (0xFF50, "p"), (0xFF51, "q"),
    (0xFF52, "r"), (0xFF53, "s"), (0xFF54, "t"), (0xFF55, "u"),
    (0xFF56, "v"), (0xFF57, "w"), (0xFF58, "x"), (0xFF59, "y"),
    (0xFF5A, "z"), (0xFF5B, "{"), (0xFF5C, "|"), (0xFF5D, "}"),
    (0xFF5E, "~"), (0xFF5F, "\u{2985}"), (0xFF60, "\u{2986}"), (0xFF61, "\u{3002}"),
    (0xFF62, "\u{300c}"), (0xFF63, "\u{300d}"), (0xFF64, "\u{3001}"), (0xFF65, "\u{30fb}"),
    (0xFF66, "\u{30f2}"), (0xFF67, "\u{30a1}"), (0xFF68, "\u{30a3}"), (0xFF69, "\u{30a5}"),
    (0xFF6A, "\u{30a7}"), (0xFF6B, "\u{30a9}"), (0xFF6C, "\u{30e3}"), (0xFF6D, "\u{30e5}"),
    (0xFF6E, "\u{30e7}"), (0xFF6F, "\u{30c3}"), (0xFF70, "\u{30fc}"), (0xFF71, "\u{30a2}"),
    (0xFF72, "\u{30a4}"), (0xFF73, "\u{30a6}"), (0xFF74, "\u{30a8}"), (0xFF75, "\u{30aa}"),
    (0xFF76, "\u{30ab}"), (0xFF77, "\u{30ad}"), (0xFF78, "\u{30af}"), (0xFF79, "\u{30b1}"),
    (0xFF7A, "\u{30b3}"), (0xFF7B, "\u{30b5}"), (0xFF7C, "\u{30b7}"), (0xFF7D, "\u{30b9}"),
    (0xFF7E, "\u{30bb}"), (0xFF7F, "\u{30bd}"), (0xFF80, "\u{30bf}"), (0xFF81, "\u{30c1}"),
    (0xFF82, "\u{30c4}"), (0xFF83, "\u{30c6}"), (0xFF84, "\u{30c8}"), (0xFF85, "\u{30ca}"),
    (0xFF86, "\u{30cb}"), (0xFF87, "\u{30cc}"), (0xFF88, "\u{30cd}"), (0xFF89, "\u{30ce}"),
    (0xFF8A, "\u{30cf}"), (0xFF8B, "\u{30d2}"), (0xFF8C, "\u{30d5}"), (0xFF8D, "\u{30d8}"),
    (0xFF8E, "\u{30db}"), (0xFF8F, "\u{30de}"), (0xFF90, "\u{30df}"), (0xFF91, "\u{30e0}"),
    (0xFF92, "\u{30e1}"), (0xFF93, "\u{30e2}"), (0xFF94, "\u{30e4}"), (0xFF95, "\u{30e6}"),
    (0xFF96, "\u{30e8}"), (0xFF97, "\u{30e9}"), (0xFF98, "\u{30ea}"), (0xFF99, "\u{30eb}"),
    (0xFF9A, "\u{30ec}"), (0xFF9B, "\u{30ed}"), (0xFF9C, "\u{30ef}"), (0xFF9D, "\u{30f3}"),
    (0xFF9E, "\u{3099}"), (0xFF9F, "\u{309a}"), (0xFFA0, "\u{1160}"), (0xFFA1, "\u{1100}"),
    (0xFFA2, "\u{1101}"), (0xFFA3, "\u{11aa}"), (0xFFA4, "\u{1102}"), (0xFFA5, "\u{11ac}"),
    (0xFFA6, "\u{11ad}"), (0xFFA7, "\u{1103}"), (0xFFA8, "\u{1104}"), (0xFFA9, "\u{1105}"),
    (0xFFAA, "\u{11b0}"), (0xFFAB, "\u{11b1}"), (0xFFAC, "\u{11b2}"), (0xFFAD, "\u{11b3}"),
    (0xFFAE, "\u{11b4}"), (0xFFAF, "\u{11b5}"), (0xFFB0, "\u{111a}"), (0xFFB1, "\u{1106}"),
    (0xFFB2, "\u{1107}"), (0xFFB3, "\u{1108}"), (0xFFB4, "\u{1121}"), (0xFFB5, "\u{1109}"),
    (0xFFB6, "\u{110a}"), (0xFFB7, "\u{110b}"), (0xFFB8, "\u{110c}"), (0xFFB9, "\u{110d}"),
    (0xFFBA, "\u{110e}"), (0xFFBB, "\u{110f}"), (0xFFBC, "\u{1110}"), (0xFFBD, "\u{1111}"),
    (0xFFBE, "\u{1112}"), (0xFFC2, "\u{1161}"), (0xFFC3, "\u{1162}"), (0xFFC4, "\u{1163}"),
    (0xFFC5, "\u{1164}"), (0xFFC6, "\u{1165}"), (0xFFC7, "\u{1166}"), (0xFFCA, "\u{1167}"),
    (0xFFCB, "\u{1168}"), (0xFFCC, "\u{1169}"), (0xFFCD, "\u{116a}"), (0xFFCE, "\u{116b}"),
    (0xFFCF, "\u{116c}"), (0xFFD2, "\u{116d}"), (0xFFD3, "\u{116e}"), (0xFFD4, "\u{116f}"),
    (0xFFD5, "\u{1170}"), (0xFFD6, "\u{1171}"), (0xFFD7, "\u{1172}"), (0xFFDA, "\u{1173}"),
    (0xFFDB, "\u{1174}"), (0xFFDC, "\u{1175}"), (0xFFE0, "\u{a2}"), (0xFFE1, "\u{a3}"),
    (0xFFE2, "\u{ac}"), (0xFFE3, " \u{304}"), (0xFFE4, "\u{a6}"), (0xFFE5, "\u{a5}"),
    (0xFFE6, "\u{20a9}"), (0xFFE8, "\u{2502}"), (0xFFE9, "\u{2190}"), (0xFFEA, "\u{2191}"),
    (0xFFEB, "\u{2192}"), (0xFFEC, "\u{2193}"), (0xFFED, "\u{25a0}"), (0xFFEE, "\u{25cb}"),
    (0x105C9, "\u{105d2}\u{307}"), (0x105E4, "\u{105da}\u{307}"), (0x10781, "\u{2d0}"), (0x10782, "\u{2d1}"),
    (0x10783, "\u{e6}"), (0x10784, "\u{299}"), (0x10785, "\u{253}"), (0x10787, "\u{2a3}"),
    (0x10788, "\u{ab66}"), (0x10789, "\u{2a5}"), (0x1078A, "\u{2a4}"), (0x1078B, "\u{256}"),
    (0x1078C, "\u{257}"), (0x1078D, "\u{1d91}"), (0x1078E, "\u{258}"), (0x1078F, "\u{25e}"),
    (0x10790, "\u{2a9}"), (0x10791, "\u{264}"), (0x10792, "\u{262}"), (0x10793, "\u{260}"),
    (0x10794, "\u{29b}"), (0x10795, "\u{127}"), (0x10796, "\u{29c}"), (0x10797, "\u{267}"),
    (0x10798, "\u{284}"), (0x10799, "\u{2aa}"), (0x1079A, "\u{2ab}"), (0x1079B, "\u{26c}"),
    (0x1079C, "\u{1df04}"), (0x1079D, "\u{a78e}"), (0x1079E, "\u{26e}"), (0x1079F, "\u{1df05}"),
    (0x107A0, "\u{28e}"), (0x107A1, "\u{1df06}"), (0x107A2, "\u{f8}"), (0x107A3, "\u{276}"),
    (0x107A4, "\u{277}"), (0x107A5, "q"), (0x107A6, "\u{27a}"), (0x107A7, "\u{1df08}"),
    (0x107A8, "\u{27d}"), (0x107A9, "\u{27e}"), (0x107AA, "\u{280}"), (0x107AB, "\u{2a8}"),
    (0x107AC, "\u{2a6}"), (0x107AD, "\u{ab67}"), (0x107AE, "\u{2a7}"), (0x107AF, "\u{288}"),
    (0x107B0, "\u{2c71}"), (0x107B2, "\u{28f}"), (0x107B3, "\u{2a1}"), (0x107B4, "\u{2a2}"),
    (0x107B5, "\u{298}"), (0x107B6, "\u{1c0}"), (0x107B7, "\u{1c1}"), (0x107B8, "\u{1c2}"),
    (0x107B9, "\u{1df0a}"), (0x107BA, "\u{1df1e}"), (0x1109A, "\u{11099}\u{110ba}"), (0x1109C, "\u{1109b}\u{110ba}"),
    (0x110AB, "\u{110a5}\u{110ba}"), (0x1112E, "\u{11131}\u{11127}"), (0x1112F, "\u{11132}\u{11127}"), (0x1134B, "\u{11347}\u{1133e}"),
    (0x1134C, "\u{11347}\u{11357}"), (0x11383, "\u{11382}\u{113c9}"), (0x11385, "\u{11384}\u{113bb}"), (0x1138E, "\u{1138b}\u{113c2}"),
    (0x11391, "\u{11390}\u{113c9}"), (0x113C5, "\u{113c2}\u{113c2}"), (0x113C7, "\u{113c2}\u{113b8}"), (0x113C8, "\u{113c2}\u{113c9}"),
    (0x114BB, "\u{114b9}\u{114ba}"), (0x114BC, "\u{114b9}\u{114b0}"), (0x114BE, "\u{114b9}\u{114bd}"), (0x115BA, "\u{115b8}\u{115af}"),
    (0x115BB, "\u{115b9}\u{115af}"), (0x11938, "\u{11935}\u{11930}"), (0x16121, "\u{1611e}\u{1611e}"), (0x16122, "\u{1611e}\u{16129}"),
    (0x16123, "\u{1611e}\u{1611f}"), (0x16124, "\u{16129}\u{1611f}"), (0x16125, "\u{1611e}\u{16120}"), (0x16126, "\u{1611e}\u{1611e}\u{1611f}"),
    (0x16127, "\u{1611e}\u{16129}\u{1611f}"), (0x16128, "\u{1611e}\u{1611e}\u{16120}"), (0x16D68, "\u{16d67}\u{16d67}"), (0x16D69, "\u{16d63}\u{16d67}"),
    (0x16D6A, "\u{16d63}\u{16d67}\u{16d67}"), (0x1CCD6, "A"), (0x1CCD7, "B"), (0x1CCD8, "C"),
    (0x1CCD9, "D"), (0x1CCDA, "E"), (0x1CCDB, "F"), (0x1CCDC, "G"),
    (0x1CCDD, "H"), (0x1CCDE, "I"), (0x1CCDF, "J"), (0x1CCE0, "K"),
    (0x1CCE1, "L"), (0x1CCE2, "M"), (0x1CCE3, "N"), (0x1CCE4, "O"),
    (0x1CCE5, "P"), (0x1CCE6, "Q"), (0x1CCE7, "R"), (0x1CCE8, "S"),
    (0x1CCE9, "T"), (0x1CCEA, "U"), (0x1CCEB, "V"), (0x1CCEC, "W"),
    (0x1CCED, "X"), (0x1CCEE, "Y"), (0x1CCEF, "Z"), (0x1CCF0, "0"),
    (0x1CCF1, "1"), (0x1CCF2, "2"), (0x1CCF3, "3"), (0x1CCF4, "4"),
    (0x1CCF5, "5"), (0x1CCF6, "6"), (0x1CCF7, "7"), (0x1CCF8, "8"),
    (0x1CCF9, "9"), (0x1D15E, "\u{1d157}\u{1d165}"), (0x1D15F, "\u{1d158}\u{1d165}"), (0x1D160, "\u{1d158}\u{1d165}\u{1d16e}"),
    (0x1D161, "\u{1d158}\u{1d165}\u{1d16f}"), (0x1D162, "\u{1d158}\u{1d165}\u{1d170}"), (0x1D163, "\u{1d158}\u{1d165}\u{1d171}"), (0x1D164, "\u{1d158}\u{1d165}\u{1d172}"),
    (0x1D1BB, "\u{1d1b9}\u{1d165}"), (0x1D1BC, "\u{1d1ba}\u{1d165}"), (0x1D1BD, "\u{1d1b9}\u{1d165}\u{1d16e}"), (0x1D1BE, "\u{1d1ba}\u{1d165}\u{1d16e}"),
    (0x1D1BF, "\u{1d1b9}\u{1d165}\u{1d16f}"), (0x1D1C0, "\u{1d1ba}\u{1d165}\u{1d16f}"), (0x1D400, "A"), (0x1D401, "B"),
    (0x1D402, "C"), (0x1D403, "D"), (0x1D404, "E"), (0x1D405, "F"),
    (0x1D406, "G"), (0x1D407, "H"), (0x1D408, "I"), (0x1D409, "J"),
    (0x1D40A, "K"), (0x1D40B, "L"), (0x1D40C, "M"), (0x1D40D, "N"),
    (0x1D40E, "O"), (0x1D40F, "P"), (0x1D410, "Q"), (0x1D411, "R"),
    (0x1D412, "S"), (0x1D413, "T"), (0x1D414, "U"), (0x1D415, "V"),
    (0x1D416, "W"), (0x1D417, "X"), (0x1D418, "Y"), (0x1D419, "Z"),
    (0x1D41A, "a"), (0x1D41B, "b"), (0x1D41C, "c"), (0x1D41D, "d"),
    (0x1D41E, "e"), (0x1D41F, "f"), (0x1D420, "g"), (0x1D421, "h"),
    (0x1D422, "i"), (0x1D423, "j"), (0x1D424, "k"), (0x1D425, "l"),
    (0x1D426, "m"), (0x1D427, "n"), (0x1D428, "o"), (0x1D429, "p"),
    (0x1D42A, "q"), (0x1D42B, "r"), (0x1D42C, "s"), (0x1D42D, "t"),
    (0x1D42E, "u"), (0x1D42F, "v"), (0x1D430, "w"), (0x1D431, "x"),
    (0x1D432, "y"), (0x1D433, "z"), (0x1D434, "A"), (0x1D435, "B"),
    (0x1D436, "C"), (0x1D437, "D"), (0x1D438, "E"), (0x1D439, "F"),
    (0x1D43A, "G"), (0x1D43B, "H"), (0x1D43C, "I"), (0x1D43D, "J"),
    (0x1D43E, "K"), (0x1D43F, "L"), (0x1D440, "M"), (0x1D441, "N"),
    (0x1D442, "O"), (0x1D443, "P"), (0x1D444, "Q"), (0x1D445, "R"),
    (0x1D446, "S"), (0x1D447, "T"), (0x1D448, "U"), (0x1D449, "V"),
    (0x1D44A, "W"), (0x1D44B, "X"), (0x1D44C, "Y"), (0x1D44D, "Z"),
    (0x1D44E, "a"), (0x1D44F, "b"), (0x1D450, "c"), (0x1D451, "d"),
    (0x1D452, "e"), (0x1D453, "f"), (0x1D454, "g"), (0x1D456, "i"),
    (0x1D457, "j"), (0x1D458, "k"), (0x1D459, "l"), (0x1D45A, "m"),
    (0x1D45B, "n"), (0x1D45C, "o"), (0x1D45D, "p"), (0x1D45E, "q"),
    (0x1D45F, "r"), (0x1D460, "s"), (0x1D461, "t"), (0x1D462, "u"),
    (0x1D463, "v"), (0x1D464, "w"), (0x1D465, "x"), (0x1D466, "y"),
    (0x1D467, "z"), (0x1D468, "A"), (0x1D469, "B"), (0x1D46A, "C"),
    (0x1D46B, "D"), (0x1D46C, "E"), (0x1D46D, "F"), (0x1D46E, "G"),
    (0x1D46F, "H"), (0x1D470, "I"), (0x1D471, "J"), (0x1D472, "K"),
    (0x1D473, "L"), (0x1D474, "M"), (0x1D475, "N"), (0x1D476, "O"),
    (0x1D477, "P"), (0x1D478, "Q"), (0x1D479, "R"), (0x1D47A, "S"),
    (0x1D47B, "T"), (0x1D47C, "U"), (0x1D47D, "V"), (0x1D47E, "W"),
    (0x1D47F, "X"), (0x1D480, "Y"), (0x1D481, "Z"), (0x1D482, "a"),
    (0x1D483, "b"), (0x1D484, "c"), (0x1D485, "d"), (0x1D486, "e"),
    (0x1D487, "f"), (0x1D488, "g"), (0x1D489, "h"), (0x1D48A, "i"),
    (0x1D48B, "j"), (0x1D48C, "k"), (0x1D48D, "l"), (0x1D48E, "m"),
    (0x1D48F, "n"), (0x1D490, "o"), (0x1D491, "p"), (0x1D492, "q"),
    (0x1D493, "r"), (0x1D494, "s"), (0x1D495, "t"), (0x1D496, "u"),
    (0x1D497, "v"), (0x1D498, "w"), (0x1D499, "x"), (0x1D49A, "y"),
    (0x1D49B, "z"), (0x1D49C, "A"), (0x1D49E, "C"), (0x1D49F, "D"),
    (0x1D4A2, "G"), (0x1D4A5, "J"), (0x1D4A6, "K"), (0x1D4A9, "N"),
    (0x1D4AA, "O"), (0x1D4AB, "P"), (0x1D4AC, "Q"), (0x1D4AE, "S"),
    (0x1D4AF, "T"), (0x1D4B0, "U"), (0x1D4B1, "V"), (0x1D4B2, "W"),
    (0x1D4B3, "X"), (0x1D4B4, "Y"), (0x1D4B5, "Z"), (0x1D4B6, "a"),
    (0x1D4B7, "b"), (0x1D4B8, "c"), (0x1D4B9, "d"), (0x1D4BB, "f"),
    (0x1D4BD, "h"), (0x1D4BE, "i"), (0x1D4BF, "j"), (0x1D4C0, "k"),
    (0x1D4C1, "l"), (0x1D4C2, "m"), (0x1D4C3, "n"), (0x1D4C5, "p"),
    (0x1D4C6, "q"), (0x1D4C7, "r"), (0x1D4C8, "s"), (0x1D4C9, "t"),
    (0x1D4CA, "u"), (0x1D4CB, "v"), (0x1D4CC, "w"), (0x1D4CD, "x"),
    (0x1D4CE, "y"), (0x1D4CF, "z"), (0x1D4D0, "A"), (0x1D4D1, "B"),
    (0x1D4D2, "C"), (0x1D4D3, "D"), (0x1D4D4, "E"), (0x1D4D5, "F"),
    (0x1D4D6, "G"), (0x1D4D7, "H"), (0x1D4D8, "I"), (0x1D4D9, "J"),
    (0x1D4DA, "K"), (0x1D4DB, "L"), (0x1D4DC, "M"), (0x1D4DD, "N"),
    (0x1D4DE, "O"), (0x1D4DF, "P"), (0x1D4E0, "Q"), (0x1D4E1, "R"),
    (0x1D4E2, "S"), (0x1D4E3, "T"), (0x1D4E4, "U"), (0x1D4E5, "V"),
    (0x1D4E6, "W"), (0x1D4E7, "X"), (0x1D4E8, "Y"), (0x1D4E9, "Z"),
    (0x1D4EA, "a"), (0x1D4EB, "b"), (0x1D4EC, "c"), (0x1D4ED, "d"),
    (0x1D4EE, "e"), (0x1D4EF, "f"), (0x1D4F0, "g"), (0x1D4F1, "h"),
    (0x1D4F2, "i"), (0x1D4F3, "j"), (0x1D4F4, "k"), (0x1D4F5, "l"),
    (0x1D4F6, "m"), (0x1D4F7, "n"), (0x1D4F8, "o"), (0x1D4F9, "p"),
    (0x1D4FA, "q"), (0x1D4FB, "r"), (0x1D4FC, "s"), (0x1D4FD, "t"),
    (0x1D4FE, "u"), (0x1D4FF, "v"), (0x1D500, "w"), (0x1D501, "x"),
    (0x1D502, "y"), (0x1D503, "z"), (0x1D504, "A"), (0x1D505, "B"),
    (0x1D507, "D"), (0x1D508, "E"), (0x1D509, "F"), (0x1D50A, "G"),
    (0x1D50D, "J"), (0x1D50E, "K"), (0x1D50F, "L"), (0x1D510, "M"),
    (0x1D511, "N"), (0x1D512, "O"), (0x1D513, "P"), (0x1D514, "Q"),
    (0x1D516, "S"), (0x1D517, "T"), (0x1D518, "U"), (0x1D519, "V"),
    (0x1D51A, "W"), (0x1D51B, "X"), (0x1D51C, "Y"), (0x1D51E, "a"),
    (0x1D51F, "b"), (0x1D520, "c"), (0x1D521, "d"), (0x1D522, "e"),
    (0x1D523, "f"), (0x1D524, "g"), (0x1D525, "h"), (0x1D526, "i"),
    (0x1D527, "j"), (0x1D528, "k"), (0x1D529, "l"), (0x1D52A, "m"),
    (0x1D52B, "n"), (0x1D52C, "o"), (0x1D52D, "p"), (0x1D52E, "q"),
    (0x1D52F, "r"), (0x1D530, "s"), (0x1D531, "t"), (0x1D532, "u"),
    (0x1D533, "v"), (0x1D534, "w"), (0x1D535, "x"), (0x1D536, "y"),
    (0x1D537, "z"), (0x1D538, "A"), (0x1D539, "B"), (0x1D53B, "D"),
    (0x1D53C, "E"), (0x1D53D, "F"), (0x1D53E, "G"), (0x1D540, "I"),
    (0x1D541, "J"), (0x1D542, "K"), (0x1D543, "L"), (0x1D544, "M"),
    (0x1D546, "O"), (0x1D54A, "S"), (0x1D54B, "T"), (0x1D54C, "U"),
    (0x1D54D, "V"), (0x1D54E, "W"), (0x1D54F, "X"), (0x1D550, "Y"),
    (0x1D552, "a"), (0x1D553, "b"), (0x1D554, "c"), (0x1D555, "d"),
    (0x1D556, "e"), (0x1D557, "f"), (0x1D558, "g"), (0x1D559, "h"),
    (0x1D55A, "i"), (0x1D55B, "j"), (0x1D55C, "k"), (0x1D55D, "l"),
    (0x1D55E, "m"), (0x1D55F, "n"), (0x1D560, "o"), (0x1D561, "p"),
    (0x1D562, "q"), (0x1D563, "r"), (0x1D564, "s"), (0x1D565, "t"),
    (0x1D566, "u"), (0x1D567, "v"), (0x1D568, "w"), (0x1D569, "x"),
    (0x1D56A, "y"), (0x1D56B, "z"), (0x1D56C, "A"), (0x1D56D, "B"),
    (0x1D56E, "C"), (0x1D56F, "D"), (0x1D570, "E"), (0x1D571, "F"),
    (0x1D572, "G"), (0x1D573, "H"), (0x1D574, "I"), (0x1D575, "J"),
    (0x1D576, "K"), (0x1D577, "L"), (0x1D578, "M"), (0x1D579, "N"),
    (0x1D57A, "O"), (0x1D57B, "P"), (0x1D57C, "Q"), (0x1D57D, "R"),
    (0x1D57E, "S"), (0x1D57F, "T"), (0x1D580, "U"), (0x1D581, "V"),
    (0x1D582, "W"), (0x1D583, "X"), (0x1D584, "Y"), (0x1D585, "Z"),
    (0x1D586, "a"), (0x1D587, "b"), (0x1D588, "c"), (0x1D589, "d"),
    (0x1D58A, "e"), (0x1D58B, "f"), (0x1D58C, "g"), (0x1D58D, "h"),
    (0x1D58E, "i"), (0x1D58F, "j"), (0x1D590, "k"), (0x1D591, "l"),
    (0x1D592, "m"), (0x1D593, "n"), (0x1D594, "o"), (0x1D595, "p"),
    (0x1D596, "q"), (0x1D597, "r"), (0x1D598, "s"), (0x1D599, "t"),
    (0x1D59A, "u"), (0x1D59B, "v"), (0x1D59C, "w"), (0x1D59D, "x"),
    (0x1D59E, "y"), (0x1D59F, "z"), (0x1D5A0, "A"), (0x1D5A1, "B"),
    (0x1D5A2, "C"), (0x1D5A3, "D"), (0x1D5A4, "E"), (0x1D5A5, "F"),
    (0x1D5A6, "G"), (0x1D5A7, "H"), (0x1D5A8, "I"), (0x1D5A9, "J"),
    (0x1D5AA, "K"), (0x1D5AB, "L"), (0x1D5AC, "M"), (0x1D5AD, "N"),
    (0x1D5AE, "O"), (0x1D5AF, "P"), (0x1D5B0, "Q"), (0x1D5B1, "R"),
    (0x1D5B2, "S"), (0x1D5B3, "T"), (0x1D5B4, "U"), (0x1D5B5, "V"),
    (0x1D5B6, "W"), (0x1D5B7, "X"), (0x1D5B8, "Y"), (0x1D5B9, "Z"),
    (0x1D5BA, "a"), (0x1D5BB, "b"), (0x1D5BC, "c"), (0x1D5BD, "d"),
    (0x1D5BE, "e"), (0x1D5BF, "f"), (0x1D5C0, "g"), (0x1D5C1, "h"),
    (0x1D5C2, "i"), (0x1D5C3, "j"), (0x1D5C4, "k"), (0x1D5C5, "l"),
    (0x1D5C6, "m"), (0x1D5C7, "n"), (0x1D5C8, "o"), (0x1D5C9, "p"),
    (0x1D5CA, "q"), (0x1D5CB, "r"), (0x1D5CC, "s"), (0x1D5CD, "t"),
    (0x1D5CE, "u"), (0x1D5CF, "v"), (0x1D5D0, "w"), (0x1D5D1, "x"),
    (0x1D5D2, "y"), (0x1D5D3, "z"), (0x1D5D4, "A"), (0x1D5D5, "B"),
    (0x1D5D6, "C"), (0x1D5D7, "D"), (0x1D5D8, "E"), (0x1D5D9, "F"),
    (0x1D5DA, "G"), (0x1D5DB, "H"), (0x1D5DC, "I"), (0x1D5DD, "J"),
    (0x1D5DE, "K"), (0x1D5DF, "L"), (0x1D5E0, "M"), (0x1D5E1, "N"),
    (0x1D5E2, "O"), (0x1D5E3, "P"), (0x1D5E4, "Q"), (0x1D5E5, "R"),
    (0x1D5E6, "S"), (0x1D5E7, "T"), (0x1D5E8, "U"), (0x1D5E9, "V"),
    (0x1D5EA, "W"), (0x1D5EB, "X"), (0x1D5EC, "Y"), (0x1D5ED, "Z"),
    (0x1D5EE, "a"), (0x1D5EF, "b"), (0x1D5F0, "c"), (0x1D5F1, "d"),
    (0x1D5F2, "e"), (0x1D5F3, "f"), (0x1D5F4, "g"), (0x1D5F5, "h"),
    (0x1D5F6, "i"), (0x1D5F7, "j"), (0x1D5F8, "k"), (0x1D5F9, "l"),
    (0x1D5FA, "m"), (0x1D5FB, "n"), (0x1D5FC, "o"), (0x1D5FD, "p"),
    (0x1D5FE, "q"), (0x1D5FF, "r"), (0x1D600, "s"), (0x1D601, "t"),
    (0x1D602, "u"), (0x1D603, "v"), (0x1D604, "w"), (0x1D605, "x"),
    (0x1D606, "y"), (0x1D607, "z"), (0x1D608, "A"), (0x1D609, "B"),
    (0x1D60A, "C"), (0x1D60B, "D"), (0x1D60C, "E"), (0x1D60D, "F"),
    (0x1D60E, "G"), (0x1D60F, "H"), (0x1D610, "I"), (0x1D611, "J"),
    (0x1D612, "K"), (0x1D613, "L"), (0x1D614, "M"), (0x1D615, "N"),
    (0x1D616, "O"), (0x1D617, "P"), (0x1D618, "Q"), (0x1D619, "R"),
    (0x1D61A, "S"), (0x1D61B, "T"), (0x1D61C, "U"), (0x1D61D, "V"),
    (0x1D61E, "W"), (0x1D61F, "X"), (0x1D620, "Y"), (0x1D621, "Z"),
    (0x1D622, "a"), (0x1D623, "b"), (0x1D624, "c"), (0x1D625, "d"),
    (0x1D626, "e"), (0x1D627, "f"), (0x1D628, "g"), (0x1D629, "h"),
    (0x1D62A, "i"), (0x1D62B, "j"), (0x1D62C, "k"), (0x1D62D, "l"),
    (0x1D62E, "m"), (0x1D62F, "n"), (0x1D630, "o"), (0x1D631, "p"),
    (0x1D632, "q"), (0x1D633, "r"), (0x1D634, "s"), (0x1D635, "t"),
    (0x1D636, "u"), (0x1D637, "v"), (0x1D638, "w"), (0x1D639, "x"),
    (0x1D63A, "y"), (0x1D63B, "z"), (0x1D63C, "A"), (0x1D63D, "B"),
    (0x1D63E, "C"), (0x1D63F, "D"), (0x1D640, "E"), (0x1D641, "F"),
    (0x1D642, "G"), (0x1D643, "H"), (0x1D644, "I"), (0x1D645, "J"),
    (0x1D646, "K"), (0x1D647, "L"), (0x1D648, "M"), (0x1D649, "N"),
    (0x1D64A, "O"), (0x1D64B, "P"), (0x1D64C, "Q"), (0x1D64D, "R"),
    (0x1D64E, "S"), (0x1D64F, "T"), (0x1D650, "U"), (0x1D651, "V"),
    (0x1D652, "W"), (0x1D653, "X"), (0x1D654, "Y"), (0x1D655, "Z"),
    (0x1D656, "a"), (0x1D657, "b"), (0x1D658, "c"), (0x1D659, "d"),
    (0x1D65A, "e"), (0x1D65B, "f"), (0x1D65C, "g"), (0x1D65D, "h"),
    (0x1D65E, "i"), (0x1D65F, "j"), (0x1D660, "k"), (0x1D661, "l"),
    (0x1D662, "m"), (0x1D663, "n"), (0x1D664, "o"), (0x1D665, "p"),
    (0x1D666, "q"), (0x1D667, "r"), (0x1D668, "s"), (0x1D669, "t"),
    (0x1D66A, "u"), (0x1D66B, "v"), (0x1D66C, "w"), (0x1D66D, "x"),
    (0x1D66E, "y"), (0x1D66F, "z"), (0x1D670, "A"), (0x1D671, "B"),
    (0x1D672, "C"), (0x1D673, "D"), (0x1D674, "E"), (0x1D675, "F"),
    (0x1D676, "G"), (0x1D677, "H"), (0x1D678, "I"), (0x1D679, "J"),
    (0x1D67A, "K"), (0x1D67B, "L"), (0x1D67C, "M"), (0x1D67D, "N"),
    (0x1D67E, "O"), (0x1D67F, "P"), (0x1D680, "Q"), (0x1D681, "R"),
    (0x1D682, "S"), (0x1D683, "T"), (0x1D684, "U"), (0x1D685, "V"),
    (0x1D686, "W"), (0x1D687, "X"), (0x1D688, "Y"), (0x1D689, "Z"),
    (0x1D68A, "a"), (0x1D68B, "b"), (0x1D68C, "c"), (0x1D68D, "d"),
    (0x1D68E, "e"), (0x1D68F, "f"), (0x1D690, "g"), (0x1D691, "h"),
    (0x1D692, "i"), (0x1D693, "j"), (0x1D694, "k"), (0x1D695, "l"),
    (0x1D696, "m"), (0x1D697, "n"), (0x1D698, "o"), (0x1D699, "p"),
    (0x1D69A, "q"), (0x1D69B, "r"), (0x1D69C, "s"), (0x1D69D, "t"),
    (0x1D69E, "u"), (0x1D69F, "v"), (0x1D6A0, "w"), (0x1D6A1, "x"),
    (0x1D6A2, "y"), (0x1D6A3, "z"), (0x1D6A4, "\u{131}"), (0x1D6A5, "\u{237}"),
    (0x1D6A8, "\u{391}"), (0x1D6A9, "\u{392}"), (0x1D6AA, "\u{393}"), (0x1D6AB, "\u{394}"),
    (0x1D6AC, "\u{395}"), (0x1D6AD, "\u{396}"), (0x1D6AE, "\u{397}"), (0x1D6AF, "\u{398}"),
    (0x1D6B0, "\u{399}"), (0x1D6B1, "\u{39a}"), (0x1D6B2, "\u{39b}"), (0x1D6B3, "\u{39c}"),
    (0x1D6B4, "\u{39d}"), (0x1D6B5, "\u{39e}"), (0x1D6B6, "\u{39f}"), (0x1D6B7, "\u{3a0}"),
    (0x1D6B8, "\u{3a1}"), (0x1D6B9, "\u{398}"), (0x1D6BA, "\u{3a3}"), (0x1D6BB, "\u{3a4}"),
    (0x1D6BC, "\u{3a5}"), (0x1D6BD, "\u{3a6}"), (0x1D6BE, "\u{3a7}"), (0x1D6BF, "\u{3a8}"),
    (0x1D6C0, "\u{3a9}"), (0x1D6C1, "\u{2207}"), (0x1D6C2, "\u{3b1}"), (0x1D6C3, "\u{3b2}"),
    (0x1D6C4, "\u{3b3}"), (0x1D6C5, "\u{3b4}"), (0x1D6C6, "\u{3b5}"), (0x1D6C7, "\u{3b6}"),
    (0x1D6C8, "\u{3b7}"), (0x1D6C9, "\u{3b8}"), (0x1D6CA, "\u{3b9}"), (0x1D6CB, "\u{3ba}"),
    (0x1D6CC, "\u{3bb}"), (0x1D6CD, "\u{3bc}"), (0x1D6CE, "\u{3bd}"), (0x1D6CF, "\u{3be}"),
    (0x1D6D0, "\u{3bf}"), (0x1D6D1, "\u{3c0}"), (0x1D6D2, "\u{3c1}"), (0x1D6D3, "\u{3c2}"),
    (0x1D6D4, "\u{3c3}"), (0x1D6D5, "\u{3c4}"), (0x1D6D6, "\u{3c5}"), (0x1D6D7, "\u{3c6}"),
    (0x1D6D8, "\u{3c7}"), (0x1D6D9, "\u{3c8}"), (0x1D6DA, "\u{3c9}"), (0x1D6DB, "\u{2202}"),
    (0x1D6DC, "\u{3b5}"), (0x1D6DD, "\u{3b8}"), (0x1D6DE, "\u{3ba}"), (0x1D6DF, "\u{3c6}"),
    (0x1D6E0, "\u{3c1}"), (0x1D6E1, "\u{3c0}"), (0x1D6E2, "\u{391}"), (0x1D6E3, "\u{392}"),
    (0x1D6E4, "\u{393}"), (0x1D6E5, "\u{394}"), (0x1D6E6, "\u{395}"), (0x1D6E7, "\u{396}"),
    (0x1D6E8, "\u{397}"), (0x1D6E9, "\u{398}"), (0x1D6EA, "\u{399}"), (0x1D6EB, "\u{39a}"),
    (0x1D6EC, "\u{39b}"), (0x1D6ED, "\u{39c}"), (0x1D6EE, "\u{39d}"), (0x1D6EF, "\u{39e}"),
    (0x1D6F0, "\u{39f}"), (0x1D6F1, "\u{3a0}"), (0x1D6F2, "\u{3a1}"), (0x1D6F3, "\u{398}"),
    (0x1D6F4, "\u{3a3}"), (0x1D6F5, "\u{3a4}"), (0x1D6F6, "\u{3a5}"), (0x1D6F7, "\u{3a6}"),
    (0x1D6F8, "\u{3a7}"), (0x1D6F9, "\u{3a8}"), (0x1D6FA, "\u{3a9}"), (0x1D6FB, "\u{2207}"),
    (0x1D6FC, "\u{3b1}"), (0x1D6FD, "\u{3b2}"), (0x1D6FE, "\u{3b3}"), (0x1D6FF, "\u{3b4}"),
    (0x1D700, "\u{3b5}"), (0x1D701, "\u{3b6}"), (0x1D702, "\u{3b7}"), (0x1D703, "\u{3b8}"),
    (0x1D704, "\u{3b9}"), (0x1D705, "\u{3ba}"), (0x1D706, "\u{3bb}"), (0x1D707, "\u{3bc}"),
    (0x1D708, "\u{3bd}"), (0x1D709, "\u{3be}"), (0x1D70A, "\u{3bf}"), (0x1D70B, "\u{3c0}"),
    (0x1D70C, "\u{3c1}"), (0x1D70D, "\u{3c2}"), (0x1D70E, "\u{3c3}"), (0x1D70F, "\u{3c4}"),
    (0x1D710, "\u{3c5}"), (0x1D711, "\u{3c6}"), (0x1D712, "\u{3c7}"), (0x1D713, "\u{3c8}"),
    (0x1D714, "\u{3c9}"), (0x1D715, "\u{2202}"), (0x1D716, "\u{3b5}"), (0x1D717, "\u{3b8}"),
    (0x1D718, "\u{3ba}"), (0x1D719, "\u{3c6}"), (0x1D71A, "\u{3c1}"), (0x1D71B, "\u{3c0}"),
    (0x1D71C, "\u{391}"), (0x1D71D, "\u{392}"), (0x1D71E, "\u{393}"), (0x1D71F, "\u{394}"),
    (0x1D720, "\u{395}"), (0x1D721, "\u{396}"), (0x1D722, "\u{397}"), (0x1D723, "\u{398}"),
    (0x1D724, "\u{399}"), (0x1D725, "\u{39a}"), (0x1D726, "\u{39b}"), (0x1D727, "\u{39c}"),
    (0x1D728, "\u{39d}"), (0x1D729, "\u{39e}"), (0x1D72A, "\u{39f}"), (0x1D72B, "\u{3a0}"),
    (0x1D72C, "\u{3a1}"), (0x1D72D, "\u{398}"), (0x1D72E, "\u{3a3}"), (0x1D72F, "\u{3a4}"),
    (0x1D730, "\u{3a5}"), (0x1D731, "\u{3a6}"), (0x1D732, "\u{3a7}"), (0x1D733, "\u{3a8}"),
    (0x1D734, "\u{3a9}"), (0x1D735, "\u{2207}"), (0x1D736, "\u{3b1}"), (0x1D737, "\u{3b2}"),
    (0x1D738, "\u{3b3}"), (0x1D739, "\u{3b4}"), (0x1D73A, "\u{3b5}"), (0x1D73B, "\u{3b6}"),
    (0x1D73C, "\u{3b7}"), (0x1D73D, "\u{3b8}"), (0x1D73E, "\u{3b9}"), (0x1D73F, "\u{3ba}"),
    (0x1D740, "\u{3bb}"), (0x1D741, "\u{3bc}"), (0x1D742, "\u{3bd}"), (0x1D743, "\u{3be}"),
    (0x1D744, "\u{3bf}"), (0x1D745, "\u{3c0}"), (0x1D746, "\u{3c1}"), (0x1D747, "\u{3c2}"),
    (0x1D748, "\u{3c3}"), (0x1D749, "\u{3c4}"), (0x1D74A, "\u{3c5}"), (0x1D74B, "\u{3c6}"),
    (0x1D74C, "\u{3c7}"), (0x1D74D, "\u{3c8}"), (0x1D74E, "\u{3c9}"), (0x1D74F, "\u{2202}"),
    (0x1D750, "\u{3b5}"), (0x1D751, "\u{3b8}"), (0x1D752, "\u{3ba}"), (0x1D753, "\u{3c6}"),
    (0x1D754, "\u{3c1}"), (0x1D755, "\u{3c0}"), (0x1D756, "\u{391}"), (0x1D757, "\u{392}"),
    (0x1D758, "\u{393}"), (0x1D759, "\u{394}"), (0x1D75A, "\u{395}"), (0x1D75B, "\u{396}"),
    (0x1D75C, "\u{397}"), (0x1D75D, "\u{398}"), (0x1D75E, "\u{399}"), (0x1D75F, "\u{39a}"),
    (0x1D760, "\u{39b}"), (0x1D761, "\u{39c}"), (0x1D762, "\u{39d}"), (0x1D763, "\u{39e}"),
    (0x1D764, "\u{39f}"), (0x1D765, "\u{3a0}"), (0x1D766, "\u{3a1}"), (0x1D767, "\u{398}"),
    (0x1D768, "\u{3a3}"), (0x1D769, "\u{3a4}"), (0x1D76A, "\u{3a5}"), (0x1D76B, "\u{3a6}"),
    (0x1D76C, "\u{3a7}"), (0x1D76D, "\u{3a8}"), (0x1D76E, "\u{3a9}"), (0x1D76F, "\u{2207}"),
    (0x1D770, "\u{3b1}"), (0x1D771, "\u{3b2}"), (0x1D772, "\u{3b3}"), (0x1D773, "\u{3b4}"),
    (0x1D774, "\u{3b5}"), (0x1D775, "\u{3b6}"), (0x1D776, "\u{3b7}"), (0x1D777, "\u{3b8}"),
    (0x1D778, "\u{3b9}"), (0x1D779, "\u{3ba}"), (0x1D77A, "\u{3bb}"), (0x1D77B, "\u{3bc}"),
    (0x1D77C, "\u{3bd}"), (0x1D77D, "\u{3be}"), (0x1D77E, "\u{3bf}"), (0x1D77F, "\u{3c0}"),
    (0x1D780, "\u{3c1}"), (0x1D781, "\u{3c2}"), (0x1D782, "\u{3c3}"), (0x1D783, "\u{3c4}"),
    (0x1D784, "\u{3c5}"), (0x1D785, "\u{3c6}"), (0x1D786, "\u{3c7}"), (0x1D787, "\u{3c8}"),
    (0x1D788, "\u{3c9}"), (0x1D789, "\u{2202}"), (0x1D78A, "\u{3b5}"), (0x1D78B, "\u{3b8}"),
    (0x1D78C, "\u{3ba}"), (0x1D78D, "\u{3c6}"), (0x1D78E, "\u{3c1}"), (0x1D78F, "\u{3c0}"),
    (0x1D790, "\u{391}"), (0x1D791, "\u{392}"), (0x1D792, "\u{393}"), (0x1D793, "\u{394}"),
    (0x1D794, "\u{395}"), (0x1D795, "\u{396}"), (0x1D796, "\u{397}"), (0x1D797, "\u{398}"),
    (0x1D798, "\u{399}"), (0x1D799, "\u{39a}"), (0x1D79A, "\u{39b}"), (0x1D79B, "\u{39c}"),
    (0x1D79C, "\u{39d}"), (0x1D79D, "\u{39e}"), (0x1D79E, "\u{39f}"), (0x1D79F, "\u{3a0}"),
    (0x1D7A0, "\u{3a1}"), (0x1D7A1, "\u{398}"), (0x1D7A2, "\u{3a3}"), (0x1D7A3, "\u{3a4}"),
    (0x1D7A4, "\u{3a5}"), (0x1D7A5, "\u{3a6}"), (0x1D7A6, "\u{3a7}"), (0x1D7A7, "\u{3a8}"),
    (0x1D7A8, "\u{3a9}"), (0x1D7A9, "\u{2207}"), (0x1D7AA, "\u{3b1}"), (0x1D7AB, "\u{3b2}"),
    (0x1D7AC, "\u{3b3}"), (0x1D7AD, "\u{3b4}"), (0x1D7AE, "\u{3b5}"), (0x1D7AF, "\u{3b6}"),
    (0x1D7B0, "\u{3b7}"), (0x1D7B1, "\u{3b8}"), (0x1D7B2, "\u{3b9}"), (0x1D7B3, "\u{3ba}"),
    (0x1D7B4, "\u{3bb}"), (0x1D7B5, "\u{3bc}"), (0x1D7B6, "\u{3bd}"), (0x1D7B7, "\u{3be}"),
    (0x1D7B8, "\u{3bf}"), (0x1D7B9, "\u{3c0}"), (0x1D7BA, "\u{3c1}"), (0x1D7BB, "\u{3c2}"),
    (0x1D7BC, "\u{3c3}"), (0x1D7BD, "\u{3c4}"), (0x1D7BE, "\u{3c5}"), (0x1D7BF, "\u{3c6}"),
    (0x1D7C0, "\u{3c7}"), (0x1D7C1, "\u{3c8}"), (0x1D7C2, "\u{3c9}"), (0x1D7C3, "\u{2202}"),
    (0x1D7C4, "\u{3b5}"), (0x1D7C5, "\u{3b8}"), (0x1D7C6, "\u{3ba}"), (0x1D7C7, "\u{3c6}"),
    (0x1D7C8, "\u{3c1}"), (0x1D7C9, "\u{3c0}"), (0x1D7CA, "\u{3dc}"), (0x1D7CB, "\u{3dd}"),
    (0x1D7CE, "0"), (0x1D7CF, "1"), (0x1D7D0, "2"), (0x1D7D1, "3"),
    (0x1D7D2, "4"), (0x1D7D3, "5"), (0x1D7D4, "6"), (0x1D7D5, "7"),
    (0x1D7D6, "8"), (0x1D7D7, "9"), (0x1D7D8, "0"), (0x1D7D9, "1"),
    (0x1D7DA, "2"), (0x1D7DB, "3"), (0x1D7DC, "4"), (0x1D7DD, "5"),
    (0x1D7DE, "6"), (0x1D7DF, "7"), (0x1D7E0, "8"), (0x1D7E1, "9"),
    (0x1D7E2, "0"), (0x1D7E3, "1"), (0x1D7E4, "2"), (0x1D7E5, "3"),
    (0x1D7E6, "4"), (0x1D7E7, "5"), (0x1D7E8, "6"), (0x1D7E9, "7"),
    (0x1D7EA, "8"), (0x1D7EB, "9"), (0x1D7EC, "0"), (0x1D7ED, "1"),
    (0x1D7EE, "2"), (0x1D7EF, "3"), (0x1D7F0, "4"), (0x1D7F1, "5"),
    (0x1D7F2, "6"), (0x1D7F3, "7"), (0x1D7F4, "8"), (0x1D7F5, "9"),
    (0x1D7F6, "0"), (0x1D7F7, "1"), (0x1D7F8, "2"), (0x1D7F9, "3"),
    (0x1D7FA, "4"), (0x1D7FB, "5"), (0x1D7FC, "6"), (0x1D7FD, "7"),
    (0x1D7FE, "8"), (0x1D7FF, "9"), (0x1E030, "\u{430}"), (0x1E031, "\u{431}"),
    (0x1E032, "\u{432}"), (0x1E033, "\u{433}"), (0x1E034, "\u{434}"), (0x1E035, "\u{435}"),
    (0x1E036, "\u{436}"), (0x1E037, "\u{437}"), (0x1E038, "\u{438}"), (0x1E039, "\u{43a}"),
    (0x1E03A, "\u{43b}"), (0x1E03B, "\u{43c}"), (0x1E03C, "\u{43e}"), (0x1E03D, "\u{43f}"),
    (0x1E03E, "\u{440}"), (0x1E03F, "\u{441}"), (0x1E040, "\u{442}"), (0x1E041, "\u{443}"),
    (0x1E042, "\u{444}"), (0x1E043, "\u{445}"), (0x1E044, "\u{446}"), (0x1E045, "\u{447}"),
    (0x1E046, "\u{448}"), (0x1E047, "\u{44b}"), (0x1E048, "\u{44d}"), (0x1E049, "\u{44e}"),
    (0x1E04A, "\u{a689}"), (0x1E04B, "\u{4d9}"), (0x1E04C, "\u{456}"), (0x1E04D, "\u{458}"),
    (0x1E04E, "\u{4e9}"), (0x1E04F, "\u{4af}"), (0x1E050, "\u{4cf}"), (0x1E051, "\u{430}"),
    (0x1E052, "\u{431}"), (0x1E053, "\u{432}"), (0x1E054, "\u{433}"), (0x1E055, "\u{434}"),
    (0x1E056, "\u{435}"), (0x1E057, "\u{436}"), (0x1E058, "\u{437}"), (0x1E059, "\u{438}"),
    (0x1E05A, "\u{43a}"), (0x1E05B, "\u{43b}"), (0x1E05C, "\u{43e}"), (0x1E05D, "\u{43f}"),
    (0x1E05E, "\u{441}"), (0x1E05F, "\u{443}"), (0x1E060, "\u{444}"), (0x1E061, "\u{445}"),
    (0x1E062, "\u{446}"), (0x1E063, "\u{447}"), (0x1E064, "\u{448}"), (0x1E065, "\u{44a}"),
    (0x1E066, "\u{44b}"), (0x1E067, "\u{491}"), (0x1E068, "\u{456}"), (0x1E069, "\u{455}"),
    (0x1E06A, "\u{45f}"), (0x1E06B, "\u{4ab}"), (0x1E06C, "\u{a651}"), (0x1E06D, "\u{4b1}"),
    (0x1EE00, "\u{627}"), (0x1EE01, "\u{628}"), (0x1EE02, "\u{62c}"), (0x1EE03, "\u{62f}"),
    (0x1EE05, "\u{648}"), (0x1EE06, "\u{632}"), (0x1EE07, "\u{62d}"), (0x1EE08, "\u{637}"),
    (0x1EE09, "\u{64a}"), (0x1EE0A, "\u{643}"), (0x1EE0B, "\u{644}"), (0x1EE0C, "\u{645}"),
    (0x1EE0D, "\u{646}"), (0x1EE0E, "\u{633}"), (0x1EE0F, "\u{639}"), (0x1EE10, "\u{641}"),
    (0x1EE11, "\u{635}"), (0x1EE12, "\u{642}"), (0x1EE13, "\u{631}"), (0x1EE14, "\u{634}"),
    (0x1EE15, "\u{62a}"), (0x1EE16, "\u{62b}"), (0x1EE17, "\u{62e}"), (0x1EE18, "\u{630}"),
    (0x1EE19, "\u{636}"), (0x1EE1A, "\u{638}"), (0x1EE1B, "\u{63a}"), (0x1EE1C, "\u{66e}"),
    (0x1EE1D, "\u{6ba}"), (0x1EE1E, "\u{6a1}"), (0x1EE1F, "\u{66f}"), (0x1EE21, "\u{628}"),
    (0x1EE22, "\u{62c}"), (0x1EE24, "\u{647}"), (0x1EE27, "\u{62d}"), (0x1EE29, "\u{64a}"),
    (0x1EE2A, "\u{643}"), (0x1EE2B, "\u{644}"), (0x1EE2C, "\u{645}"), (0x1EE2D, "\u{646}"),
    (0x1EE2E, "\u{633}"), (0x1EE2F, "\u{639}"), (0x1EE30, "\u{641}"), (0x1EE31, "\u{635}"),
    (0x1EE32, "\u{642}"), (0x1EE34, "\u{634}"), (0x1EE35, "\u{62a}"), (0x1EE36, "\u{62b}"),
    (0x1EE37, "\u{62e}"), (0x1EE39, "\u{636}"), (0x1EE3B, "\u{63a}"), (0x1EE42, "\u{62c}"),
    (0x1EE47, "\u{62d}"), (0x1EE49, "\u{64a}"), (0x1EE4B, "\u{644}"), (0x1EE4D, "\u{646}"),
    (0x1EE4E, "\u{633}"), (0x1EE4F, "\u{639}"), (0x1EE51, "\u{635}"), (0x1EE52, "\u{642}"),
    (0x1EE54, "\u{634}"), (0x1EE57, "\u{62e}"), (0x1EE59, "\u{636}"), (0x1EE5B, "\u{63a}"),
    (0x1EE5D, "\u{6ba}"), (0x1EE5F, "\u{66f}"), (0x1EE61, "\u{628}"), (0x1EE62, "\u{62c}"),
    (0x1EE64, "\u{647}"), (0x1EE67, "\u{62d}"), (0x1EE68, "\u{637}"), (0x1EE69, "\u{64a}"),
    (0x1EE6A, "\u{643}"), (0x1EE6C, "\u{645}"), (0x1EE6D, "\u{646}"), (0x1EE6E, "\u{633}"),
    (0x1EE6F, "\u{639}"), (0x1EE70, "\u{641}"), (0x1EE71, "\u{635}"), (0x1EE72, "\u{642}"),
    (0x1EE74, "\u{634}"), (0x1EE75, "\u{62a}"), (0x1EE76, "\u{62b}"), (0x1EE77, "\u{62e}"),
    (0x1EE79, "\u{636}"), (0x1EE7A, "\u{638}"), (0x1EE7B, "\u{63a}"), (0x1EE7C, "\u{66e}"),
    (0x1EE7E, "\u{6a1}"), (0x1EE80, "\u{627}"), (0x1EE81, "\u{628}"), (0x1EE82, "\u{62c}"),
    (0x1EE83, "\u{62f}"), (0x1EE84, "\u{647}"), (0x1EE85, "\u{648}"), (0x1EE86, "\u{632}"),
    (0x1EE87, "\u{62d}"), (0x1EE88, "\u{637}"), (0x1EE89, "\u{64a}"), (0x1EE8B, "\u{644}"),
    (0x1EE8C, "\u{645}"), (0x1EE8D, "\u{646}"), (0x1EE8E, "\u{633}"), (0x1EE8F, "\u{639}"),
    (0x1EE90, "\u{641}"), (0x1EE91, "\u{635}"), (0x1EE92, "\u{642}"), (0x1EE93, "\u{631}"),
    (0x1EE94, "\u{634}"), (0x1EE95, "\u{62a}"), (0x1EE96, "\u{62b}"), (0x1EE97, "\u{62e}"),
    (0x1EE98, "\u{630}"), (0x1EE99, "\u{636}"), (0x1EE9A, "\u{638}"), (0x1EE9B, "\u{63a}"),
    (0x1EEA1, "\u{628}"), (0x1EEA2, "\u{62c}"), (0x1EEA3, "\u{62f}"), (0x1EEA5, "\u{648}"),
    (0x1EEA6, "\u{632}"), (0x1EEA7, "\u{62d}"), (0x1EEA8, "\u{637}"), (0x1EEA9, "\u{64a}"),
    (0x1EEAB, "\u{644}"), (0x1EEAC, "\u{645}"), (0x1EEAD, "\u{646}"), (0x1EEAE, "\u{633}"),
    (0x1EEAF, "\u{639}"), (0x1EEB0, "\u{641}"), (0x1EEB1, "\u{635}"), (0x1EEB2, "\u{642}"),
    (0x1EEB3, "\u{631}"), (0x1EEB4, "\u{634}"), (0x1EEB5, "\u{62a}"), (0x1EEB6, "\u{62b}"),
    (0x1EEB7, "\u{62e}"), (0x1EEB8, "\u{630}"), (0x1EEB9, "\u{636}"), (0x1EEBA, "\u{638}"),
    (0x1EEBB, "\u{63a}"), (0x1F100, "0."), (0x1F101, "0,"), (0x1F102, "1,"),
    (0x1F103, "2,"), (0x1F104, "3,"), (0x1F105, "4,"), (0x1F106, "5,"),
    (0x1F107, "6,"), (0x1F108, "7,"), (0x1F109, "8,"), (0x1F10A, "9,"),
    (0x1F110, "(A)"), (0x1F111, "(B)"), (0x1F112, "(C)"), (0x1F113, "(D)"),
    (0x1F114, "(E)"), (0x1F115, "(F)"), (0x1F116, "(G)"), (0x1F117, "(H)"),
    (0x1F118, "(I)"), (0x1F119, "(J)"), (0x1F11A, "(K)"), (0x1F11B, "(L)"),
    (0x1F11C, "(M)"), (0x1F11D, "(N)"), (0x1F11E, "(O)"), (0x1F11F, "(P)"),
    (0x1F120, "(Q)"), (0x1F121, "(R)"), (0x1F122, "(S)"), (0x1F123, "(T)"),
    (0x1F124, "(U)"), (0x1F125, "(V)"), (0x1F126, "(W)"), (0x1F127, "(X)"),
    (0x1F128, "(Y)"), (0x1F129, "(Z)"), (0x1F12A, "\u{3014}S\u{3015}"), (0x1F12B, "C"),
    (0x1F12C, "R"), (0x1F12D, "CD"), (0x1F12E, "WZ"), (0x1F130, "A"),
    (0x1F131, "B"), (0x1F132, "C"), (0x1F133, "D"), (0x1F134, "E"),
    (0x1F135, "F"), (0x1F136, "G"), (0x1F137, "H"), (0x1F138, "I"),
    (0x1F139, "J"), (0x1F13A, "K"), (0x1F13B, "L"), (0x1F13C, "M"),
    (0x1F13D, "N"), (0x1F13E, "O"), (0x1F13F, "P"), (0x1F140, "Q"),
    (0x1F141, "R"), (0x1F142, "S"), (0x1F143, "T"), (0x1F144, "U"),
    (0x1F145, "V"), (0x1F146, "W"), (0x1F147, "X"), (0x1F148, "Y"),
    (0x1F149, "Z"), (0x1F14A, "HV"), (0x1F14B, "MV"), (0x1F14C, "SD"),
    (0x1F14D, "SS"), (0x1F14E, "PPV"), (0x1F14F, "WC"), (0x1F16A, "MC"),
    (0x1F16B, "MD"), (0x1F16C, "MR"), (0x1F190, "DJ"), (0x1F200, "\u{307b}\u{304b}"),
    (0x1F201, "\u{30b3}\u{30b3}"), (0x1F202, "\u{30b5}"), (0x1F210, "\u{624b}"), (0x1F211, "\u{5b57}"),
    (0x1F212, "\u{53cc}"), (0x1F213, "\u{30c6}\u{3099}"), (0x1F214, "\u{4e8c}"), (0x1F215, "\u{591a}"),
    (0x1F216, "\u{89e3}"), (0x1F217, "\u{5929}"), (0x1F218, "\u{4ea4}"), (0x1F219, "\u{6620}"),
    (0x1F21A, "\u{7121}"), (0x1F21B, "\u{6599}"), (0x1F21C, "\u{524d}"), (0x1F21D, "\u{5f8c}"),
    (0x1F21E, "\u{518d}"), (0x1F21F, "\u{65b0}"), (0x1F220, "\u{521d}"), (0x1F221, "\u{7d42}"),
    (0x1F222, "\u{751f}"), (0x1F223, "\u{8ca9}"), (0x1F224, "\u{58f0}"), (0x1F225, "\u{5439}"),
    (0x1F226, "\u{6f14}"), (0x1F227, "\u{6295}"), (0x1F228, "\u{6355}"), (0x1F229, "\u{4e00}"),
    (0x1F22A, "\u{4e09}"), (0x1F22B, "\u{904a}"), (0x1F22C, "\u{5de6}"), (0x1F22D, "\u{4e2d}"),
    (0x1F22E, "\u{53f3}"), (0x1F22F, "\u{6307}"), (0x1F230, "\u{8d70}"), (0x1F231, "\u{6253}"),
    (0x1F232, "\u{7981}"), (0x1F233, "\u{7a7a}"), (0x1F234, "\u{5408}"), (0x1F235, "\u{6e80}"),
    (0x1F236, "\u{6709}"), (0x1F237, "\u{6708}"), (0x1F238, "\u{7533}"), (0x1F239, "\u{5272}"),
    (0x1F23A, "\u{55b6}"), (0x1F23B, "\u{914d}"), (0x1F240, "\u{3014}\u{672c}\u{3015}"), (0x1F241, "\u{3014}\u{4e09}\u{3015}"),
    (0x1F242, "\u{3014}\u{4e8c}\u{3015}"), (0x1F243, "\u{3014}\u{5b89}\u{3015}"), (0x1F244, "\u{3014}\u{70b9}\u{3015}"), (0x1F245, "\u{3014}\u{6253}\u{3015}"),
    (0x1F246, "\u{3014}\u{76d7}\u{3015}"), (0x1F247, "\u{3014}\u{52dd}\u{3015}"), (0x1F248, "\u{3014}\u{6557}\u{3015}"), (0x1F250, "\u{5f97}"),
    (0x1F251, "\u{53ef}"), (0x1FBF0, "0"), (0x1FBF1, "1"), (0x1FBF2, "2"),
    (0x1FBF3, "3"), (0x1FBF4, "4"), (0x1FBF5, "5"), (0x1FBF6, "6"),
    (0x1FBF7, "7"), (0x1FBF8, "8"), (0x1FBF9, "9"), (0x2F800, "\u{4e3d}"),
    (0x2F801, "\u{4e38}"), (0x2F802, "\u{4e41}"), (0x2F803, "\u{20122}"), (0x2F804, "\u{4f60}"),
    (0x2F805, "\u{4fae}"), (0x2F806, "\u{4fbb}"), (0x2F807, "\u{5002}"), (0x2F808, "\u{507a}"),
    (0x2F809, "\u{5099}"), (0x2F80A, "\u{50e7}"), (0x2F80B, "\u{50cf}"), (0x2F80C, "\u{349e}"),
    (0x2F80D, "\u{2063a}"), (0x2F80E, "\u{514d}"), (0x2F80F, "\u{5154}"), (0x2F810, "\u{5164}"),
    (0x2F811, "\u{5177}"), (0x2F812, "\u{2051c}"), (0x2F813, "\u{34b9}"), (0x2F814, "\u{5167}"),
    (0x2F815, "\u{518d}"), (0x2F816, "\u{2054b}"), (0x2F817, "\u{5197}"), (0x2F818, "\u{51a4}"),
    (0x2F819, "\u{4ecc}"), (0x2F81A, "\u{51ac}"), (0x2F81B, "\u{51b5}"), (0x2F81C, "\u{291df}"),
    (0x2F81D, "\u{51f5}"), (0x2F81E, "\u{5203}"), (0x2F81F, "\u{34df}"), (0x2F820, "\u{523b}"),
    (0x2F821, "\u{5246}"), (0x2F822, "\u{5272}"), (0x2F823, "\u{5277}"), (0x2F824, "\u{3515}"),
    (0x2F825, "\u{52c7}"), (0x2F826, "\u{52c9}"), (0x2F827, "\u{52e4}"), (0x2F828, "\u{52fa}"),
    (0x2F829, "\u{5305}"), (0x2F82A, "\u{5306}"), (0x2F82B, "\u{5317}"), (0x2F82C, "\u{5349}"),
    (0x2F82D, "\u{5351}"), (0x2F82E, "\u{535a}"), (0x2F82F, "\u{5373}"), (0x2F830, "\u{537d}"),
    (0x2F831, "\u{537f}"), (0x2F832, "\u{537f}"), (0x2F833, "\u{537f}"), (0x2F834, "\u{20a2c}"),
    (0x2F835, "\u{7070}"), (0x2F836, "\u{53ca}"), (0x2F837, "\u{53df}"), (0x2F838, "\u{20b63}"),
    (0x2F839, "\u{53eb}"), (0x2F83A, "\u{53f1}"), (0x2F83B, "\u{5406}"), (0x2F83C, "\u{549e}"),
    (0x2F83D, "\u{5438}"), (0x2F83E, "\u{5448}"), (0x2F83F, "\u{5468}"), (0x2F840, "\u{54a2}"),
    (0x2F841, "\u{54f6}"), (0x2F842, "\u{5510}"), (0x2F843, "\u{5553}"), (0x2F844, "\u{5563}"),
    (0x2F845, "\u{5584}"), (0x2F846, "\u{5584}"), (0x2F847, "\u{5599}"), (0x2F848, "\u{55ab}"),
    (0x2F849, "\u{55b3}"), (0x2F84A, "\u{55c2}"), (0x2F84B, "\u{5716}"), (0x2F84C, "\u{5606}"),
    (0x2F84D, "\u{5717}"), (0x2F84E, "\u{5651}"), (0x2F84F, "\u{5674}"), (0x2F850, "\u{5207}"),
    (0x2F851, "\u{58ee}"), (0x2F852, "\u{57ce}"), (0x2F853, "\u{57f4}"), (0x2F854, "\u{580d}"),
    (0x2F855, "\u{578b}"), (0x2F856, "\u{5832}"), (0x2F857, "\u{5831}"), (0x2F858, "\u{58ac}"),
    (0x2F859, "\u{214e4}"), (0x2F85A, "\u{58f2}"), (0x2F85B, "\u{58f7}"), (0x2F85C, "\u{5906}"),
    (0x2F85D, "\u{591a}"), (0x2F85E, "\u{5922}"), (0x2F85F, "\u{5962}"), (0x2F860, "\u{216a8}"),
    (0x2F861, "\u{216ea}"), (0x2F862, "\u{59ec}"), (0x2F863, "\u{5a1b}"), (0x2F864, "\u{5a27}"),
    (0x2F865, "\u{59d8}"), (0x2F866, "\u{5a66}"), (0x2F867, "\u{36ee}"), (0x2F868, "\u{36fc}"),
    (0x2F869, "\u{5b08}"), (0x2F86A, "\u{5b3e}"), (0x2F86B, "\u{5b3e}"), (0x2F86C, "\u{219c8}"),
    (0x2F86D, "\u{5bc3}"), (0x2F86E, "\u{5bd8}"), (0x2F86F, "\u{5be7}"), (0x2F870, "\u{5bf3}"),
    (0x2F871, "\u{21b18}"), (0x2F872, "\u{5bff}"), (0x2F873, "\u{5c06}"), (0x2F874, "\u{5f53}"),
    (0x2F875, "\u{5c22}"), (0x2F876, "\u{3781}"), (0x2F877, "\u{5c60}"), (0x2F878, "\u{5c6e}"),
    (0x2F879, "\u{5cc0}"), (0x2F87A, "\u{5c8d}"), (0x2F87B, "\u{21de4}"), (0x2F87C, "\u{5d43}"),
    (0x2F87D, "\u{21de6}"), (0x2F87E, "\u{5d6e}"), (0x2F87F, "\u{5d6b}"), (0x2F880, "\u{5d7c}"),
    (0x2F881, "\u{5de1}"), (0x2F882, "\u{5de2}"), (0x2F883, "\u{382f}"), (0x2F884, "\u{5dfd}"),
    (0x2F885, "\u{5e28}"), (0x2F886, "\u{5e3d}"), (0x2F887, "\u{5e69}"), (0x2F888, "\u{3862}"),
    (0x2F889, "\u{22183}"), (0x2F88A, "\u{387c}"), (0x2F88B, "\u{5eb0}"), (0x2F88C, "\u{5eb3}"),
    (0x2F88D, "\u{5eb6}"), (0x2F88E, "\u{5eca}"), (0x2F88F, "\u{2a392}"), (0x2F890, "\u{5efe}"),
    (0x2F891, "\u{22331}"), (0x2F892, "\u{22331}"), (0x2F893, "\u{8201}"), (0x2F894, "\u{5f22}"),
    (0x2F895, "\u{5f22}"), (0x2F896, "\u{38c7}"), (0x2F897, "\u{232b8}"), (0x2F898, "\u{261da}"),
    (0x2F899, "\u{5f62}"), (0x2F89A, "\u{5f6b}"), (0x2F89B, "\u{38e3}"), (0x2F89C, "\u{5f9a}"),
    (0x2F89D, "\u{5fcd}"), (0x2F89E, "\u{5fd7}"), (0x2F89F, "\u{5ff9}"), (0x2F8A0, "\u{6081}"),
    (0x2F8A1, "\u{393a}"), (0x2F8A2, "\u{391c}"), (0x2F8A3, "\u{6094}"), (0x2F8A4, "\u{226d4}"),
    (0x2F8A5, "\u{60c7}"), (0x2F8A6, "\u{6148}"), (0x2F8A7, "\u{614c}"), (0x2F8A8, "\u{614e}"),
    (0x2F8A9, "\u{614c}"), (0x2F8AA, "\u{617a}"), (0x2F8AB, "\u{618e}"), (0x2F8AC, "\u{61b2}"),
    (0x2F8AD, "\u{61a4}"), (0x2F8AE, "\u{61af}"), (0x2F8AF, "\u{61de}"), (0x2F8B0, "\u{61f2}"),
    (0x2F8B1, "\u{61f6}"), (0x2F8B2, "\u{6210}"), (0x2F8B3, "\u{621b}"), (0x2F8B4, "\u{625d}"),
    (0x2F8B5, "\u{62b1}"), (0x2F8B6, "\u{62d4}"), (0x2F8B7, "\u{6350}"), (0x2F8B8, "\u{22b0c}"),
    (0x2F8B9, "\u{633d}"), (0x2F8BA, "\u{62fc}"), (0x2F8BB, "\u{6368}"), (0x2F8BC, "\u{6383}"),
    (0x2F8BD, "\u{63e4}"), (0x2F8BE, "\u{22bf1}"), (0x2F8BF, "\u{6422}"), (0x2F8C0, "\u{63c5}"),
    (0x2F8C1, "\u{63a9}"), (0x2F8C2, "\u{3a2e}"), (0x2F8C3, "\u{6469}"), (0x2F8C4, "\u{647e}"),
    (0x2F8C5, "\u{649d}"), (0x2F8C6, "\u{6477}"), (0x2F8C7, "\u{3a6c}"), (0x2F8C8, "\u{654f}"),
    (0x2F8C9, "\u{656c}"), (0x2F8CA, "\u{2300a}"), (0x2F8CB, "\u{65e3}"), (0x2F8CC, "\u{66f8}"),
    (0x2F8CD, "\u{6649}"), (0x2F8CE, "\u{3b19}"), (0x2F8CF, "\u{6691}"), (0x2F8D0, "\u{3b08}"),
    (0x2F8D1, "\u{3ae4}"), (0x2F8D2, "\u{5192}"), (0x2F8D3, "\u{5195}"), (0x2F8D4, "\u{6700}"),
    (0x2F8D5, "\u{669c}"), (0x2F8D6, "\u{80ad}"), (0x2F8D7, "\u{43d9}"), (0x2F8D8, "\u{6717}"),
    (0x2F8D9, "\u{671b}"), (0x2F8DA, "\u{6721}"), (0x2F8DB, "\u{675e}"), (0x2F8DC, "\u{6753}"),
    (0x2F8DD, "\u{233c3}"), (0x2F8DE, "\u{3b49}"), (0x2F8DF, "\u{67fa}"), (0x2F8E0, "\u{6785}"),
    (0x2F8E1, "\u{6852}"), (0x2F8E2, "\u{6885}"), (0x2F8E3, "\u{2346d}"), (0x2F8E4, "\u{688e}"),
    (0x2F8E5, "\u{681f}"), (0x2F8E6, "\u{6914}"), (0x2F8E7, "\u{3b9d}"), (0x2F8E8, "\u{6942}"),
    (0x2F8E9, "\u{69a3}"), (0x2F8EA, "\u{69ea}"), (0x2F8EB, "\u{6aa8}"), (0x2F8EC, "\u{236a3}"),
    (0x2F8ED, "\u{6adb}"), (0x2F8EE, "\u{3c18}"), (0x2F8EF, "\u{6b21}"), (0x2F8F0, "\u{238a7}"),
    (0x2F8F1, "\u{6b54}"), (0x2F8F2, "\u{3c4e}"), (0x2F8F3, "\u{6b72}"), (0x2F8F4, "\u{6b9f}"),
    (0x2F8F5, "\u{6bba}"), (0x2F8F6, "\u{6bbb}"), (0x2F8F7, "\u{23a8d}"), (0x2F8F8, "\u{21d0b}"),
    (0x2F8F9, "\u{23afa}"), (0x2F8FA, "\u{6c4e}"), (0x2F8FB, "\u{23cbc}"), (0x2F8FC, "\u{6cbf}"),
    (0x2F8FD, "\u{6ccd}"), (0x2F8FE, "\u{6c67}"), (0x2F8FF, "\u{6d16}"), (0x2F900, "\u{6d3e}"),
    (0x2F901, "\u{6d77}"), (0x2F902, "\u{6d41}"), (0x2F903, "\u{6d69}"), (0x2F904, "\u{6d78}"),
    (0x2F905, "\u{6d85}"), (0x2F906, "\u{23d1e}"), (0x2F907, "\u{6d34}"), (0x2F908, "\u{6e2f}"),
    (0x2F909, "\u{6e6e}"), (0x2F90A, "\u{3d33}"), (0x2F90B, "\u{6ecb}"), (0x2F90C, "\u{6ec7}"),
    (0x2F90D, "\u{23ed1}"), (0x2F90E, "\u{6df9}"), (0x2F90F, "\u{6f6e}"), (0x2F910, "\u{23f5e}"),
    (0x2F911, "\u{23f8e}"), (0x2F912, "\u{6fc6}"), (0x2F913, "\u{7039}"), (0x2F914, "\u{701e}"),
    (0x2F915, "\u{701b}"), (0x2F916, "\u{3d96}"), (0x2F917, "\u{704a}"), (0x2F918, "\u{707d}"),
    (0x2F919, "\u{7077}"), (0x2F91A, "\u{70ad}"), (0x2F91B, "\u{20525}"), (0x2F91C, "\u{7145}"),
    (0x2F91D, "\u{24263}"), (0x2F91E, "\u{719c}"), (0x2F91F, "\u{243ab}"), (0x2F920, "\u{7228}"),
    (0x2F921, "\u{7235}"), (0x2F922, "\u{7250}"), (0x2F923, "\u{24608}"), (0x2F924, "\u{7280}"),
    (0x2F925, "\u{7295}"), (0x2F926, "\u{24735}"), (0x2F927, "\u{24814}"), (0x2F928, "\u{737a}"),
    (0x2F929, "\u{738b}"), (0x2F92A, "\u{3eac}"), (0x2F92B, "\u{73a5}"), (0x2F92C, "\u{3eb8}"),
    (0x2F92D, "\u{3eb8}"), (0x2F92E, "\u{7447}"), (0x2F92F, "\u{745c}"), (0x2F930, "\u{7471}"),
    (0x2F931, "\u{7485}"), (0x2F932, "\u{74ca}"), (0x2F933, "\u{3f1b}"), (0x2F934, "\u{7524}"),
    (0x2F935, "\u{24c36}"), (0x2F936, "\u{753e}"), (0x2F937, "\u{24c92}"), (0x2F938, "\u{7570}"),
    (0x2F939, "\u{2219f}"), (0x2F93A, "\u{7610}"), (0x2F93B, "\u{24fa1}"), (0x2F93C, "\u{24fb8}"),
    (0x2F93D, "\u{25044}"), (0x2F93E, "\u{3ffc}"), (0x2F93F, "\u{4008}"), (0x2F940, "\u{76f4}"),
    (0x2F941, "\u{250f3}"), (0x2F942, "\u{250f2}"), (0x2F943, "\u{25119}"), (0x2F944, "\u{25133}"),
    (0x2F945, "\u{771e}"), (0x2F946, "\u{771f}"), (0x2F947, "\u{771f}"), (0x2F948, "\u{774a}"),
    (0x2F949, "\u{4039}"), (0x2F94A, "\u{778b}"), (0x2F94B, "\u{4046}"), (0x2F94C, "\u{4096}"),
    (0x2F94D, "\u{2541d}"), (0x2F94E, "\u{784e}"), (0x2F94F, "\u{788c}"), (0x2F950, "\u{78cc}"),
    (0x2F951, "\u{40e3}"), (0x2F952, "\u{25626}"), (0x2F953, "\u{7956}"), (0x2F954, "\u{2569a}"),
    (0x2F955, "\u{256c5}"), (0x2F956, "\u{798f}"), (0x2F957, "\u{79eb}"), (0x2F958, "\u{412f}"),
    (0x2F959, "\u{7a40}"), (0x2F95A, "\u{7a4a}"), (0x2F95B, "\u{7a4f}"), (0x2F95C, "\u{2597c}"),
    (0x2F95D, "\u{25aa7}"), (0x2F95E, "\u{25aa7}"), (0x2F95F, "\u{7aee}"), (0x2F960, "\u{4202}"),
    (0x2F961, "\u{25bab}"), (0x2F962, "\u{7bc6}"), (0x2F963, "\u{7bc9}"), (0x2F964, "\u{4227}"),
    (0x2F965, "\u{25c80}"), (0x2F966, "\u{7cd2}"), (0x2F967, "\u{42a0}"), (0x2F968, "\u{7ce8}"),
    (0x2F969, "\u{7ce3}"), (0x2F96A, "\u{7d00}"), (0x2F96B, "\u{25f86}"), (0x2F96C, "\u{7d63}"),
    (0x2F96D, "\u{4301}"), (0x2F96E, "\u{7dc7}"), (0x2F96F, "\u{7e02}"), (0x2F970, "\u{7e45}"),
    (0x2F971, "\u{4334}"), (0x2F972, "\u{26228}"), (0x2F973, "\u{26247}"), (0x2F974, "\u{4359}"),
    (0x2F975, "\u{262d9}"), (0x2F976, "\u{7f7a}"), (0x2F977, "\u{2633e}"), (0x2F978, "\u{7f95}"),
    (0x2F979, "\u{7ffa}"), (0x2F97A, "\u{8005}"), (0x2F97B, "\u{264da}"), (0x2F97C, "\u{26523}"),
    (0x2F97D, "\u{8060}"), (0x2F97E, "\u{265a8}"), (0x2F97F, "\u{8070}"), (0x2F980, "\u{2335f}"),
    (0x2F981, "\u{43d5}"), (0x2F982, "\u{80b2}"), (0x2F983, "\u{8103}"), (0x2F984, "\u{440b}"),
    (0x2F985, "\u{813e}"), (0x2F986, "\u{5ab5}"), (0x2F987, "\u{267a7}"), (0x2F988, "\u{267b5}"),
    (0x2F989, "\u{23393}"), (0x2F98A, "\u{2339c}"), (0x2F98B, "\u{8201}"), (0x2F98C, "\u{8204}"),
    (0x2F98D, "\u{8f9e}"), (0x2F98E, "\u{446b}"), (0x2F98F, "\u{8291}"), (0x2F990, "\u{828b}"),
    (0x2F991, "\u{829d}"), (0x2F992, "\u{52b3}"), (0x2F993, "\u{82b1}"), (0x2F994, "\u{82b3}"),
    (0x2F995, "\u{82bd}"), (0x2F996, "\u{82e6}"), (0x2F997, "\u{26b3c}"), (0x2F998, "\u{82e5}"),
    (0x2F999, "\u{831d}"), (0x2F99A, "\u{8363}"), (0x2F99B, "\u{83ad}"), (0x2F99C, "\u{8323}"),
    (0x2F99D, "\u{83bd}"), (0x2F99E, "\u{83e7}"), (0x2F99F, "\u{8457}"), (0x2F9A0, "\u{8353}"),
    (0x2F9A1, "\u{83ca}"), (0x2F9A2, "\u{83cc}"), (0x2F9A3, "\u{83dc}"), (0x2F9A4, "\u{26c36}"),
    (0x2F9A5, "\u{26d6b}"), (0x2F9A6, "\u{26cd5}"), (0x2F9A7, "\u{452b}"), (0x2F9A8, "\u{84f1}"),
    (0x2F9A9, "\u{84f3}"), (0x2F9AA, "\u{8516}"), (0x2F9AB, "\u{273ca}"), (0x2F9AC, "\u{8564}"),
    (0x2F9AD, "\u{26f2c}"), (0x2F9AE, "\u{455d}"), (0x2F9AF, "\u{4561}"), (0x2F9B0, "\u{26fb1}"),
    (0x2F9B1, "\u{270d2}"), (0x2F9B2, "\u{456b}"), (0x2F9B3, "\u{8650}"), (0x2F9B4, "\u{865c}"),
    (0x2F9B5, "\u{8667}"), (0x2F9B6, "\u{8669}"), (0x2F9B7, "\u{86a9}"), (0x2F9B8, "\u{8688}"),
    (0x2F9B9, "\u{870e}"), (0x2F9BA, "\u{86e2}"), (0x2F9BB, "\u{8779}"), (0x2F9BC, "\u{8728}"),
    (0x2F9BD, "\u{876b}"), (0x2F9BE, "\u{8786}"), (0x2F9BF, "\u{45d7}"), (0x2F9C0, "\u{87e1}"),
    (0x2F9C1, "\u{8801}"), (0x2F9C2, "\u{45f9}"), (0x2F9C3, "\u{8860}"), (0x2F9C4, "\u{8863}"),
    (0x2F9C5, "\u{27667}"), (0x2F9C6, "\u{88d7}"), (0x2F9C7, "\u{88de}"), (0x2F9C8, "\u{4635}"),
    (0x2F9C9, "\u{88fa}"), (0x2F9CA, "\u{34bb}"), (0x2F9CB, "\u{278ae}"), (0x2F9CC, "\u{27966}"),
    (0x2F9CD, "\u{46be}"), (0x2F9CE, "\u{46c7}"), (0x2F9CF, "\u{8aa0}"), (0x2F9D0, "\u{8aed}"),
    (0x2F9D1, "\u{8b8a}"), (0x2F9D2, "\u{8c55}"), (0x2F9D3, "\u{27ca8}"), (0x2F9D4, "\u{8cab}"),
    (0x2F9D5, "\u{8cc1}"), (0x2F9D6, "\u{8d1b}"), (0x2F9D7, "\u{8d77}"), (0x2F9D8, "\u{27f2f}"),
    (0x2F9D9, "\u{20804}"), (0x2F9DA, "\u{8dcb}"), (0x2F9DB, "\u{8dbc}"), (0x2F9DC, "\u{8df0}"),
    (0x2F9DD, "\u{208de}"), (0x2F9DE, "\u{8ed4}"), (0x2F9DF, "\u{8f38}"), (0x2F9E0, "\u{285d2}"),
    (0x2F9E1, "\u{285ed}"), (0x2F9E2, "\u{9094}"), (0x2F9E3, "\u{90f1}"), (0x2F9E4, "\u{9111}"),
    (0x2F9E5, "\u{2872e}"), (0x2F9E6, "\u{911b}"), (0x2F9E7, "\u{9238}"), (0x2F9E8, "\u{92d7}"),
    (0x2F9E9, "\u{92d8}"), (0x2F9EA, "\u{927c}"), (0x2F9EB, "\u{93f9}"), (0x2F9EC, "\u{9415}"),
    (0x2F9ED, "\u{28bfa}"), (0x2F9EE, "\u{958b}"), (0x2F9EF, "\u{4995}"), (0x2F9F0, "\u{95b7}"),
    (0x2F9F1, "\u{28d77}"), (0x2F9F2, "\u{49e6}"), (0x2F9F3, "\u{96c3}"), (0x2F9F4, "\u{5db2}"),
    (0x2F9F5, "\u{9723}"), (0x2F9F6, "\u{29145}"), (0x2F9F7, "\u{2921a}"), (0x2F9F8, "\u{4a6e}"),
    (0x2F9F9, "\u{4a76}"), (0x2F9FA, "\u{97e0}"), (0x2F9FB, "\u{2940a}"), (0x2F9FC, "\u{4ab2}"),
    (0x2F9FD, "\u{29496}"), (0x2F9FE, "\u{980b}"), (0x2F9FF, "\u{980b}"), (0x2FA00, "\u{9829}"),
    (0x2FA01, "\u{295b6}"), (0x2FA02, "\u{98e2}"), (0x2FA03, "\u{4b33}"), (0x2FA04, "\u{9929}"),
    (0x2FA05, "\u{99a7}"), (0x2FA06, "\u{99c2}"), (0x2FA07, "\u{99fe}"), (0x2FA08, "\u{4bce}"),
    (0x2FA09, "\u{29b30}"), (0x2FA0A, "\u{9b12}"), (0x2FA0B, "\u{9c40}"), (0x2FA0C, "\u{9cfd}"),
    (0x2FA0D, "\u{4cce}"), (0x2FA0E, "\u{4ced}"), (0x2FA0F, "\u{9d67}"), (0x2FA10, "\u{2a0ce}"),
    (0x2FA11, "\u{4cf8}"), (0x2FA12, "\u{2a105}"), (0x2FA13, "\u{2a20e}"), (0x2FA14, "\u{2a291}"),
    (0x2FA15, "\u{9ebb}"), (0x2FA16, "\u{4d56}"), (0x2FA17, "\u{9ef9}"), (0x2FA18, "\u{9efe}"),
    (0x2FA19, "\u{9f05}"), (0x2FA1A, "\u{9f0f}"), (0x2FA1B, "\u{9f16}"), (0x2FA1C, "\u{9f3b}"),
    (0x2FA1D, "\u{2a600}"),
];
