//! Tests for `plugin_market_gap`: the plugin market's decisions — format-1 index validation, the
//! search, filter, and order over a validated index, the index-source URL rule, and the install
//! arguments one listed version produces.
//!
//! The cases are the upstream ones, restated where the upstream suites state them
//! (`plugin-market/tests/market.spec.ts`, `plugin-market/tests/parse.spec.ts`,
//! `util/plugin-market-index/tests/plugin-market-index.spec.ts`), plus the ones whose *reason* is
//! the property under test rather than a recorded output: a search that never loses a plugin it
//! filtered for, a fuzzy radius that never exceeds two edits, and an index URL that never keeps a
//! credential.
//!
//! The expected values in the search tables were produced by running the *upstream* code — real
//! `MiniSearch` 7.2.0 with `search.ts`'s exact options — over the same three plugins, so a case that
//! fails means this port disagrees with upstream rather than with an author's expectation.
//!
//! # Why the module is compiled by path
//!
//! The crate root's wiring is a separate change, so this compiles the exact source file rather than
//! a copy. Inside a `#[path]` module the `crate::` prefix would name *this* test crate, not
//! `girder-core`, so every reference to the library goes through `girder_core::{...}` — which is
//! why the module itself imports nothing from the crate root.

#[path = "../src/plugin_market_gap.rs"]
mod plugin_market_gap;

use serde_json::json;

use plugin_market_gap::{
    IndexParseResult, IndexUrlReason, MAX_INLINE_ICON_BYTES, MAX_LIMIT, MarketIndex,
    MarketIndexError, MarketQuery, MarketSort, MarketSource, MarketVersion, QueryError,
    SearchIndex, SortDirection, Url, is_semver_range, market_install_target, nfkc,
    normalize_index_url, parse_index_body, parse_market_index, search_terms, validate_page,
};

// ============================================================================================
// Helpers
// ============================================================================================

const INDEX_URL: &str = "https://market.example/dsh/index.json";
const COMMIT: &str = "3f9c2e1d8a4b6c0e7f1a2b3c4d5e6f708192a3b4";

/// An npm version with the fields a test does not override.
fn npm_version(overrides: serde_json::Value) -> serde_json::Value {
    let mut base =
        json!({ "version": "1.2.3", "publishTimestamp": 1790476800, "source": { "type": "npm" } });
    merge(&mut base, overrides);
    base
}

/// A plugin entry with one npm version and the fields a test does not override.
fn entry(overrides: serde_json::Value) -> serde_json::Value {
    let mut base = json!({ "name": "@scope/dsh-foo", "versions": [npm_version(json!({}))] });
    merge(&mut base, overrides);
    base
}

/// A complete index document with the fields a test does not override.
///
/// The overrides use `null` for "remove this member", because a shallow merge cannot express
/// absence any other way.
fn document(overrides: serde_json::Value) -> serde_json::Value {
    let mut base = json!({
        "format": 1,
        "title": { "en": "Example Market", "zh": "示例市场" },
        "metadata": {
            "categories": { "productivity": { "title": "Productivity" } },
            "labels": {
                "official": { "title": { "en": "Official", "zh": "官方" } },
                "ui": { "title": "UI" }
            },
            "sortKeys": { "downloads": { "title": "Downloads", "order": "desc" } }
        },
        "plugins": [entry(json!({}))]
    });
    merge(&mut base, overrides);
    base
}

/// Merge `overrides` into `base`, object by object; a null override removes the member.
fn merge(base: &mut serde_json::Value, overrides: serde_json::Value) {
    match (base, overrides) {
        (serde_json::Value::Object(base), serde_json::Value::Object(overrides)) => {
            for (key, value) in overrides {
                if value.is_null() {
                    base.remove(&key);
                } else {
                    merge(base.entry(key).or_insert(serde_json::Value::Null), value);
                }
            }
        }
        (base, overrides) => *base = overrides,
    }
}

/// A parsed index, or a panic naming the violation (a refusal here is a failed test, not a case).
fn parsed(document: serde_json::Value) -> MarketIndex {
    match parse_market_index(&document, INDEX_URL) {
        Ok(index) => index,
        Err(error) => panic!("the index was refused: {error}"),
    }
}

/// The violation for a document that must be refused.
fn rejection(document: serde_json::Value) -> MarketIndexError {
    match parse_market_index(&document, INDEX_URL) {
        Ok(_) => panic!("the index was accepted"),
        Err(error) => error,
    }
}

/// The message for a plugin that must be refused.
fn entry_rejection(overrides: serde_json::Value) -> String {
    rejection(document(json!({ "plugins": [entry(overrides)] }))).to_string()
}

/// The message for a version that must be refused.
fn version_rejection(overrides: serde_json::Value) -> String {
    entry_rejection(json!({ "versions": [npm_version(overrides)] }))
}

/// A page query: index order, no text, every plugin.
fn all() -> MarketQuery {
    MarketQuery::default()
}

/// A text query in index order.
fn text(text: &str) -> MarketQuery {
    MarketQuery {
        text: Some(text.to_owned()),
        ..MarketQuery::default()
    }
}

/// The names on a result page, in result order.
fn names(index: &MarketIndex, query: &MarketQuery) -> Vec<String> {
    SearchIndex::new(index)
        .select(query)
        .expect("the page range is valid")
        .positions
        .into_iter()
        .map(|position| index.plugins[position].name.clone())
        .collect()
}

/// Upstream `SEARCHABLE`: three plugins whose fields cover every matching case.
fn searchable() -> MarketIndex {
    parsed(json!({
        "format": 1,
        "title": "Market",
        "metadata": {
            "categories": {
                "work": { "title": { "en": "Productivity", "zh": "效率" } },
                "fun": { "title": "Games" }
            },
            "labels": {
                "official": { "title": { "en": "Official", "zh": "官方" } },
                "ui": { "title": "UI" }
            },
            "sortKeys": { "downloads": { "title": "Downloads", "order": "desc" } }
        },
        "plugins": [
            entry(json!({
                "name": "dsh-weather",
                "description": { "en": "Shows the forecast without review", "ja": "天気予報を表示" },
                "author": "Kim",
                "metadata": { "category": "fun", "labels": ["ui"], "sortKeys": { "downloads": 10 } },
                "versions": [npm_version(json!({ "version": "1.0.0", "publishTimestamp": 30 }))]
            })),
            entry(json!({
                "name": "dsh-auto-review",
                "title": { "en": "Automatic review", "zh": "自动代码审查" },
                "metadata": {
                    "category": "work", "labels": ["official", "ui"], "keywords": ["lint"],
                    "sortKeys": { "downloads": 50 }
                },
                "versions": [
                    npm_version(json!({ "version": "2.0.0", "publishTimestamp": 10 })),
                    npm_version(json!({ "version": "1.0.0", "publishTimestamp": 40 }))
                ]
            })),
            entry(json!({
                "name": "dsh-notes",
                "description": "Keep a review checklist",
                "metadata": { "category": "work", "labels": ["official"] },
                "versions": [npm_version(json!({ "version": "1.0.0", "publishTimestamp": 20 }))]
            }))
        ]
    }))
}

/// The names a query over [`searchable`] returns.
fn searched(query: &MarketQuery) -> Vec<String> {
    names(&searchable(), query)
}

// ============================================================================================
// `searchTerms` — the tokenizer every other search decision rests on
// ============================================================================================

#[test]
fn splits_words_on_punctuation_and_keeps_their_order() {
    assert_eq!(search_terms("dsh-auto-review"), ["dsh", "auto", "review"]);
    assert_eq!(search_terms("hello, world!"), ["hello", "world"]);
    assert_eq!(search_terms("abc"), ["abc"]);
    assert_eq!(search_terms(""), Vec::<String>::new());
}

#[test]
fn text_without_letters_or_digits_has_no_terms() {
    // This is what makes `select` treat ` -- ` as "every plugin".
    assert_eq!(search_terms(" -- "), Vec::<String>::new());
    assert_eq!(search_terms("!!!"), Vec::<String>::new());
}

#[test]
fn a_cjk_run_yields_overlapping_pairs_and_its_last_character() {
    // The pair `审查` is what makes the single character `审` match as a prefix, and the retained
    // last character is what makes `查` match at all — the reason `searchTerms` does not split on
    // spaces alone.
    assert_eq!(search_terms("代码审查"), ["代码", "码审", "审查", "查"]);
    assert_eq!(search_terms("审"), ["审"]);
    assert_eq!(search_terms("天気"), ["天気", "気"]);
    assert_eq!(search_terms("국어"), ["국어", "어"]);
}

#[test]
fn cjk_and_latin_runs_split_at_their_boundary() {
    assert_eq!(
        search_terms("支持Markdown预览"),
        ["支持", "持", "Markdown", "预览", "览"]
    );
    assert_eq!(search_terms("Vue插件"), ["Vue", "插件", "件"]);
}

#[test]
fn script_extensions_keep_the_prolonged_sound_mark_and_iteration_mark_in_cjk_runs() {
    // `ー` and `々` are Han/Kana by *script extension*, not by general category, which is why the
    // tokenizer tests scripts rather than code-point blocks.
    assert_eq!(
        search_terms("コーヒーサーバー"),
        ["コー", "ーヒ", "ヒー", "ーサ", "サー", "ーバ", "バー", "ー"]
    );
    assert_eq!(search_terms("佐々木"), ["佐々", "々木", "木"]);
}

#[test]
fn nfkc_folds_full_width_half_width_decomposed_and_compatibility_forms() {
    // Each upstream case: full-width letters, half-width kana, a decomposed accent, a decomposed
    // Hangul syllable, and a compatibility character.
    assert_eq!(search_terms("ＡＢＣ Tools"), ["ABC", "Tools"]);
    assert_eq!(search_terms("ﾊﾟｿｺﾝ"), ["パソ", "ソコ", "コン", "ン"]);
    assert_eq!(search_terms("cafe\u{301} menu"), ["café", "menu"]);
    assert_eq!(search_terms("\u{1112}\u{1161}\u{11AB}\u{1100}\u{116e}\u{1165}"), ["한국어", "어"]);
    assert_eq!(search_terms("㈱"), ["株"]);
}

#[test]
fn nfkc_agrees_with_the_javascript_runtime_on_the_forms_that_matter() {
    assert_eq!(nfkc("ＡＢＣ"), "ABC");
    assert_eq!(nfkc("ﾊﾟｿｺﾝ"), "パソコン");
    assert_eq!(nfkc("cafe\u{301}"), "café");
    assert_eq!(nfkc("㈱"), "(株)");
    assert_eq!(nfkc("Å"), "Å");
    assert_eq!(nfkc("①"), "1");
    // Text with nothing to normalize comes back unchanged, including supplementary characters.
    assert_eq!(nfkc("dsh-auto-review"), "dsh-auto-review");
    assert_eq!(nfkc("🍰 cake"), "🍰 cake");
}

// ============================================================================================
// Filters and order — upstream `market.spec.ts`
// ============================================================================================

#[test]
fn preserves_publisher_order_before_sorting_and_through_filters() {
    assert_eq!(
        searched(&all()),
        ["dsh-weather", "dsh-auto-review", "dsh-notes"]
    );
    let labels = |labels: &[&str]| MarketQuery {
        labels: labels.iter().map(|label| (*label).to_owned()).collect(),
        ..MarketQuery::default()
    };
    assert_eq!(searched(&labels(&["ui"])), ["dsh-weather", "dsh-auto-review"]);
    assert_eq!(
        searched(&labels(&["official", "ui"])),
        ["dsh-auto-review"]
    );
    let category = |id: &str| MarketQuery {
        category: Some(id.to_owned()),
        ..MarketQuery::default()
    };
    assert_eq!(searched(&category("work")), ["dsh-auto-review", "dsh-notes"]);
    assert_eq!(
        searched(&MarketQuery {
            category: Some("fun".to_owned()),
            labels: vec!["official".to_owned()],
            ..MarketQuery::default()
        }),
        Vec::<String>::new()
    );
    // An undeclared id matches no plugin, which is what the format's declaration rule buys.
    assert_eq!(searched(&category("missing")), Vec::<String>::new());
    assert_eq!(searched(&labels(&["missing"])), Vec::<String>::new());
}

#[test]
fn matches_the_upstream_text_table() {
    // Each case is an upstream `it.each` row: label, query, expected names in index order.
    let cases: &[(&str, &str, &[&str])] = &[
        (
            "a typo of a word",
            "revew",
            &["dsh-weather", "dsh-auto-review", "dsh-notes"],
        ),
        ("a word prefix", "auto", &["dsh-auto-review"]),
        (
            "words that must all match, across fields",
            "forecast kim",
            &["dsh-weather"],
        ),
        ("a keyword", "lint", &["dsh-auto-review"]),
        ("Chinese title text", "代码审查", &["dsh-auto-review"]),
        ("one CJK character", "审", &["dsh-auto-review"]),
        ("the last CJK character of a run", "查", &["dsh-auto-review"]),
        ("Japanese description text", "天気", &["dsh-weather"]),
        ("a localized category title", "效率", &["dsh-auto-review", "dsh-notes"]),
        ("a label title", "official", &["dsh-auto-review", "dsh-notes"]),
        ("upper case", "KIM", &["dsh-weather"]),
        ("words from different plugins", "forecast lint", &[]),
        ("a short word with a typo", "dsx", &[]),
        (
            "text without words",
            " -- ",
            &["dsh-weather", "dsh-auto-review", "dsh-notes"],
        ),
    ];
    for (label, query, expected) in cases {
        assert_eq!(searched(&text(query)), *expected, "{label}: {query:?}");
    }
}

#[test]
fn ranks_by_relevance_and_orders_relevance_without_text_by_index() {
    // Upstream: `review` keeps index order without a sort, and leads with the plugin whose *title*
    // carries the word once relevance is asked for.
    assert_eq!(
        searched(&text("review")),
        ["dsh-weather", "dsh-auto-review", "dsh-notes"]
    );
    let relevance = |text: &str| MarketQuery {
        text: Some(text.to_owned()),
        sort: MarketSort::Relevance,
        ..MarketQuery::default()
    };
    assert_eq!(searched(&relevance("review"))[0], "dsh-auto-review");
    assert_eq!(searched(&relevance("checklist review")), ["dsh-notes"]);
    // Without text every score is zero, so relevance falls back to index order.
    assert_eq!(
        searched(&MarketQuery {
            sort: MarketSort::Relevance,
            ..MarketQuery::default()
        }),
        ["dsh-weather", "dsh-auto-review", "dsh-notes"]
    );
}

#[test]
fn relevance_scores_match_upstream_minisearch_exactly() {
    // The numbers come from `MiniSearch` 7.2.0 with `search.ts`'s options, over this same index.
    let index = searchable();
    let search = SearchIndex::new(&index);
    let cases: &[(&str, &[(&str, f64)])] = &[
        (
            "review",
            &[
                ("dsh-auto-review", 7.263_971),
                ("dsh-notes", 0.754_604),
                ("dsh-weather", 0.553_158),
            ],
        ),
        ("checklist review", &[("dsh-notes", 4.658_705)]),
        ("forecast kim", &[("dsh-weather", 4.401_831)]),
        ("lint", &[("dsh-auto-review", 2.093_11)]),
        ("代码审查", &[("dsh-auto-review", 50.234_636)]),
        (
            "效率",
            &[("dsh-auto-review", 2.635_102), ("dsh-notes", 2.635_102)],
        ),
        ("revew", &[("dsh-auto-review", 2.801_818), ("dsh-notes", 0.291_061), ("dsh-weather", 0.213_361)]),
    ];
    for (query, expected) in cases {
        let scores = search.scores(query);
        let mut got: Vec<(String, f64)> = scores
            .iter()
            .map(|(position, score)| (index.plugins[*position].name.clone(), *score))
            .collect();
        got.sort_by(|left, right| right.1.total_cmp(&left.1).then_with(|| left.0.cmp(&right.0)));
        let expected: Vec<(String, f64)> = expected
            .iter()
            .map(|(name, score)| ((*name).to_owned(), *score))
            .collect();
        assert_eq!(got.len(), expected.len(), "{query:?}: {got:?}");
        for ((got_name, got_score), (want_name, want_score)) in got.iter().zip(&expected) {
            assert_eq!(got_name, want_name, "{query:?}");
            assert!(
                (got_score - want_score).abs() < 1e-5,
                "{query:?} {got_name}: {got_score} != {want_score}"
            );
        }
    }
}

#[test]
fn orders_by_latest_publish_time_or_a_sort_key_placing_plugins_without_one_last() {
    let by = |sort: MarketSort| MarketQuery {
        sort,
        ..MarketQuery::default()
    };
    // `dsh-auto-review` publishes at 40 (its latest), `dsh-weather` at 30, `dsh-notes` at 20.
    assert_eq!(
        searched(&by(MarketSort::Published {
            direction: SortDirection::Desc
        })),
        ["dsh-auto-review", "dsh-weather", "dsh-notes"]
    );
    assert_eq!(
        searched(&by(MarketSort::Published {
            direction: SortDirection::Asc
        })),
        ["dsh-notes", "dsh-weather", "dsh-auto-review"]
    );
    let key = |id: &str, direction: SortDirection| MarketSort::Key {
        id: id.to_owned(),
        direction,
    };
    assert_eq!(
        searched(&by(key("downloads", SortDirection::Desc))),
        ["dsh-auto-review", "dsh-weather", "dsh-notes"]
    );
    assert_eq!(
        searched(&by(key("downloads", SortDirection::Asc))),
        ["dsh-weather", "dsh-auto-review", "dsh-notes"]
    );
    // An id the index does not declare sorts every plugin as valueless, so index order survives.
    assert_eq!(
        searched(&by(key("missing", SortDirection::Asc))),
        ["dsh-weather", "dsh-auto-review", "dsh-notes"]
    );
}

#[test]
fn keeps_index_order_among_equal_sort_key_values_and_among_plugins_without_one() {
    let valued = |name: &str, downloads: Option<f64>| {
        let metadata = match downloads {
            Some(downloads) => json!({ "sortKeys": { "downloads": downloads } }),
            None => json!({}),
        };
        entry(json!({ "name": name, "metadata": metadata }))
    };
    let index = parsed(json!({
        "format": 1,
        "title": "Sortable",
        "metadata": { "sortKeys": { "downloads": { "title": "Downloads", "order": "desc" } } },
        "plugins": [
            valued("a", None), valued("b", Some(1.0)), valued("c", None), valued("d", Some(2.0)),
            valued("e", Some(1.0)), valued("f", Some(0.0)), entry(json!({ "name": "h" })), entry(json!({ "name": "i" }))
        ]
    }));
    let ascending = names(
        &index,
        &MarketQuery {
            sort: MarketSort::Key {
                id: "downloads".to_owned(),
                direction: SortDirection::Asc,
            },
            ..MarketQuery::default()
        },
    );
    assert_eq!(ascending, ["f", "b", "e", "d", "a", "c", "h", "i"]);
    let descending = names(
        &index,
        &MarketQuery {
            sort: MarketSort::Key {
                id: "downloads".to_owned(),
                direction: SortDirection::Desc,
            },
            ..MarketQuery::default()
        },
    );
    assert_eq!(descending, ["d", "b", "e", "f", "a", "c", "h", "i"]);
}

#[test]
fn zeros_are_measured_values_while_absent_and_null_sort_keys_are_not() {
    let index = parsed(json!({
        "format": 1,
        "title": "Zero",
        "metadata": { "sortKeys": { "downloads": { "title": "D", "order": "desc" } } },
        "plugins": [
            entry(json!({ "name": "zero", "metadata": { "sortKeys": { "downloads": 0 } } })),
            entry(json!({ "name": "null", "metadata": { "sortKeys": { "downloads": null } } })),
            entry(json!({ "name": "absent", "metadata": {} }))
        ]
    }));
    assert_eq!(
        index.plugins[0].metadata.sort_keys.get("downloads"),
        Some(&0.0)
    );
    assert!(index.plugins[1].metadata.sort_keys.is_empty());
    assert!(index.plugins[2].metadata.sort_keys.is_empty());
}

// ============================================================================================
// Paging
// ============================================================================================

#[test]
fn pages_matches_with_their_total_and_the_next_offset() {
    let index = searchable();
    let search = SearchIndex::new(&index);
    let first = search
        .select(&MarketQuery {
            offset: 0,
            limit: 2,
            ..MarketQuery::default()
        })
        .expect("a valid page");
    assert_eq!(first.total, 3);
    assert_eq!(first.next_offset, Some(2));
    let names: Vec<&str> = first
        .positions
        .iter()
        .map(|position| index.plugins[*position].name.as_str())
        .collect();
    assert_eq!(names, ["dsh-weather", "dsh-auto-review"]);
    let last = search
        .select(&MarketQuery {
            offset: 2,
            limit: 2,
            ..MarketQuery::default()
        })
        .expect("a valid page");
    assert_eq!(last.total, 3);
    assert_eq!(last.next_offset, None);
    assert_eq!(last.positions.len(), 1);
    // An offset past the end is an empty page, not an error.
    let past = search
        .select(&MarketQuery {
            offset: 5,
            limit: 2,
            ..MarketQuery::default()
        })
        .expect("a valid page");
    assert!(past.positions.is_empty());
    assert_eq!(past.total, 3);
    assert_eq!(past.next_offset, None);
}

#[test]
fn refuses_a_page_range_outside_the_wire_limits() {
    for (offset, limit, field) in [
        (-1, 10, "offset"),
        (0, 0, "limit"),
        (0, 101, "limit"),
    ] {
        let error = validate_page(offset, limit).expect_err("the range is out of bounds");
        assert_eq!(
            error,
            QueryError {
                field,
                reason: "range"
            }
        );
    }
    assert_eq!(validate_page(0, 1), Ok((0, 1)));
    assert_eq!(validate_page(0, 100), Ok((0, 100)));
    assert_eq!(validate_page(7, MAX_LIMIT as i64), Ok((7, MAX_LIMIT)));
}

#[test]
fn select_refuses_a_limit_outside_the_wire_limits() {
    let index = searchable();
    let search = SearchIndex::new(&index);
    for limit in [0, MAX_LIMIT + 1] {
        let error = search
            .select(&MarketQuery {
                limit,
                ..MarketQuery::default()
            })
            .expect_err("the limit is out of range");
        assert_eq!(error.field, "limit");
    }
}

#[test]
fn counts_a_page_from_the_filtered_matches_not_from_every_plugin() {
    let index = searchable();
    let search = SearchIndex::new(&index);
    let page = search
        .select(&MarketQuery {
            category: Some("work".to_owned()),
            limit: 1,
            ..MarketQuery::default()
        })
        .expect("a valid page");
    assert_eq!(page.total, 2);
    assert_eq!(page.next_offset, Some(1));
}

// ============================================================================================
// Format-1 validation — upstream `plugin-market-index.spec.ts`
// ============================================================================================

#[test]
fn accepts_a_complete_index_filling_absent_groups_and_resolving_relative_icons() {
    let index = parsed(document(json!({
        "description": "Community bundles",
        "extra": true,
        "plugins": [
            entry(json!({
                "title": { "en": "Foo", "zh-cn": "甲" },
                "description": "Does foo",
                "icon": "icons/foo.svg",
                "author": "Jane Doe",
                "homepage": "https://github.com/jane/dsh-foo#readme",
                "repository": {
                    "url": "https://github.com/jane/dsh-plugins",
                    "directory": "packages/foo",
                    "type": "git"
                },
                "metadata": {
                    "category": "productivity",
                    "keywords": ["review"],
                    "labels": ["official", "ui"],
                    "sortKeys": { "downloads": 12 },
                    "extra": 1
                },
                "unknown": "ignored",
                "versions": [
                    npm_version(json!({ "version": "1.2.3-rc.1", "publishTimestamp": 1 })),
                    npm_version(json!({
                        "source": { "type": "npm", "registry": "https://registry.example/", "note": "x" },
                        "engines": { "dsh": ">=0.2.0", "node": ">=22" }
                    })),
                    npm_version(json!({ "version": "1.10.0", "publishTimestamp": 2, "engines": {} }))
                ]
            })),
            json!({
                "name": "dsh-bar",
                "icon": "data:image/png;base64,iVBORw==",
                "versions": [{
                    "version": "1.0.0-rc.1+build.5",
                    "publishTimestamp": 0,
                    "source": {
                        "type": "git",
                        "url": "https://gitlab.example/group/sub/repo",
                        "commit": COMMIT,
                        "path": "packages/bar"
                    }
                }]
            })
        ]
    })));
    assert_eq!(index.format, 1);
    assert_eq!(
        index.title,
        plugin_market_gap::LocalizedText::Languages(
            [
                ("en".to_owned(), "Example Market".to_owned()),
                ("zh".to_owned(), "示例市场".to_owned())
            ]
            .into_iter()
            .collect()
        )
    );
    assert_eq!(
        index.description,
        Some(plugin_market_gap::LocalizedText::Plain(
            "Community bundles".to_owned()
        ))
    );
    let foo = &index.plugins[0];
    // A relative icon resolves against the index URL's directory.
    assert_eq!(
        foo.icon.as_deref(),
        Some("https://market.example/dsh/icons/foo.svg")
    );
    assert_eq!(foo.author.as_deref(), Some("Jane Doe"));
    // A fragment on a display-only homepage survives; it names a section, not a resource.
    assert_eq!(
        foo.homepage.as_deref(),
        Some("https://github.com/jane/dsh-foo#readme")
    );
    assert_eq!(
        foo.repository,
        Some(plugin_market_gap::MarketRepository {
            url: "https://github.com/jane/dsh-plugins".to_owned(),
            directory: Some("packages/foo".to_owned()),
        })
    );
    assert_eq!(foo.metadata.category.as_deref(), Some("productivity"));
    assert_eq!(foo.metadata.keywords, ["review"]);
    assert_eq!(foo.metadata.labels, ["official", "ui"]);
    assert_eq!(foo.metadata.sort_keys.get("downloads"), Some(&12.0));
    // Versions come back in descending precedence: 1.10.0, then 1.2.3, then 1.2.3-rc.1.
    let versions: Vec<&str> = foo
        .versions
        .iter()
        .map(|version| version.version.as_str())
        .collect();
    assert_eq!(versions, ["1.10.0", "1.2.3", "1.2.3-rc.1"]);
    assert_eq!(
        foo.versions[1].source,
        MarketSource::Npm {
            registry: Some("https://registry.example/".to_owned())
        }
    );
    // `engines.node` is not part of the format and is dropped.
    assert_eq!(
        foo.versions[1].engines.dsh.as_deref(),
        Some(">=0.2.0")
    );
    let bar = &index.plugins[1];
    assert_eq!(
        bar.icon.as_deref(),
        Some("data:image/png;base64,iVBORw==")
    );
    assert!(bar.metadata.keywords.is_empty());
    assert!(bar.metadata.labels.is_empty());
    assert!(bar.metadata.sort_keys.is_empty());
    assert_eq!(
        bar.versions[0].source,
        MarketSource::Git {
            url: "https://gitlab.example/group/sub/repo".to_owned(),
            commit: COMMIT.to_owned(),
            path: Some("packages/bar".to_owned()),
        }
    );
}

#[test]
fn ignores_extra_object_fields_while_validating_known_discriminators() {
    let original = document(json!({}));
    let extended = document(json!({
        "future": { "nested": true },
        "metadata": {
            "future": [],
            "categories": { "productivity": { "title": "Productivity", "future": null } },
            "labels": { "official": { "title": { "en": "Official", "zh": "官方" }, "future": false }, "ui": { "title": "UI" } },
            "sortKeys": { "downloads": { "title": "Downloads", "order": "desc", "future": {} } }
        },
        "plugins": [entry(json!({
            "future": [],
            "source": { "type": "npm", "future": false }
        }))]
    }));
    assert_eq!(parsed(extended), parsed(original));
    assert_eq!(rejection(document(json!({ "format": 2, "future": true }))).path, "$.format");
    assert!(
        version_rejection(json!({ "source": { "type": "future", "extra": true } }))
            .contains(".source.type")
    );
    assert!(
        entry_rejection(json!({ "metadata": { "sortKeys": { "future": null } } }))
            .contains("references undeclared id")
    );
}

#[test]
fn normalizes_absent_and_null_plugin_metadata_without_inventing_zero() {
    for metadata in [json!({}), json!({ "category": null, "keywords": null, "labels": null, "sortKeys": null })] {
        let plugin = entry(json!({ "metadata": metadata }));
        let index = parsed(document(json!({ "plugins": [plugin] })));
        assert_eq!(index.plugins[0].metadata, plugin_market_gap::MarketPluginMetadata::default());
    }
    let index = parsed(document(
        json!({ "plugins": [entry(json!({ "metadata": { "sortKeys": { "downloads": 0 } } }))] }),
    ));
    assert_eq!(index.plugins[0].metadata.sort_keys.get("downloads"), Some(&0.0));
    assert!(
        entry_rejection(json!({ "metadata": { "sortKeys": { "downloads": "n/a" } } }))
            .contains("must be a finite number")
    );
}

#[test]
fn preserves_free_form_keywords_including_order_duplicates_and_empty_strings() {
    for keywords in [json!([]), json!(["market-specific", " npm ", "中文", "", "market-specific"])] {
        let index = parse_market_index(
            &json!({ "format": 1, "title": "Keywords", "plugins": [entry(json!({ "metadata": { "keywords": keywords.clone() } }))] }),
            INDEX_URL,
        )
        .expect("keywords need no declaration");
        assert_eq!(
            index.plugins[0].metadata.keywords,
            keywords
                .as_array()
                .expect("an array")
                .iter()
                .filter_map(serde_json::Value::as_str)
                .map(str::to_owned)
                .collect::<Vec<String>>()
        );
    }
}

#[test]
fn rejects_keywords_that_are_not_a_string_array_with_the_offending_path() {
    for keywords in [json!("review"), json!({})] {
        assert_eq!(
            entry_rejection(json!({ "metadata": { "keywords": keywords } })),
            "$.plugins[0].metadata.keywords must be an array"
        );
    }
    assert_eq!(
        entry_rejection(json!({ "metadata": { "keywords": ["review", 1] } })),
        "$.plugins[0].metadata.keywords[1] must be a string"
    );
}

#[test]
fn accepts_a_git_source_at_the_repository_root() {
    let index = parsed(document(json!({
        "plugins": [entry(json!({ "versions": [npm_version(json!({
            "source": { "type": "git", "url": "https://github.com/o/r.git", "commit": COMMIT }
        }))] }))]
    })));
    assert_eq!(
        index.plugins[0].versions[0].source,
        MarketSource::Git {
            url: "https://github.com/o/r.git".to_owned(),
            commit: COMMIT.to_owned(),
            path: None,
        }
    );
}

#[test]
fn accepts_an_index_without_metadata_or_plugins() {
    let index = parsed(json!({ "format": 1, "title": "Empty", "plugins": [] }));
    assert_eq!(index.metadata, plugin_market_gap::MarketIndexMetadata::default());
    assert!(index.plugins.is_empty());
    assert_eq!(index.title.variants(), ["Empty"]);
}

#[test]
fn accepts_absolute_https_icons_origin_relative_http_icons_and_padded_data_icons() {
    let local = "http://127.0.0.1:8080/index.json";
    let index = parse_market_index(
        &json!({
            "format": 1,
            "title": "Icons",
            "plugins": [
                entry(json!({ "name": "a", "icon": "https://cdn.example/a.png" })),
                entry(json!({ "name": "b", "icon": "/b.png" })),
                entry(json!({ "name": "c", "icon": "data:image/svg+xml;base64,PHN2Zz4=" }))
            ]
        }),
        local,
    )
    .expect("the icons resolve");
    let icons: Vec<&str> = index
        .plugins
        .iter()
        .filter_map(|plugin| plugin.icon.as_deref())
        .collect();
    assert_eq!(
        icons,
        [
            "https://cdn.example/a.png",
            "http://127.0.0.1:8080/b.png",
            "data:image/svg+xml;base64,PHN2Zz4="
        ]
    );
}

#[test]
fn rejects_root_violations_with_their_json_path() {
    assert_eq!(rejection(json!([])).to_string(), "$ must be an object");
    assert_eq!(rejection(document(json!({ "format": 2 }))).path, "$.format");
    assert_eq!(
        rejection(document(json!({ "title": "  " }))).to_string(),
        "$.title must be a string with a non-whitespace character"
    );
    assert_eq!(
        rejection(document(json!({ "title": { "zh": "市场" } }))).to_string(),
        "$.title must contain en"
    );
    assert_eq!(
        rejection(document(json!({ "title": { "EN": "Market" } }))).to_string(),
        "$.title.EN must be keyed by a lowercase language id"
    );
    assert_eq!(
        rejection(document(json!({ "plugins": {} }))).to_string(),
        "$.plugins must be an array"
    );
    assert_eq!(
        rejection(document(json!({ "metadata": [] }))).to_string(),
        "$.metadata must be an object"
    );
    assert_eq!(
        rejection(document(json!({ "metadata": { "labels": { "": { "title": "x" } } } }))).to_string(),
        "$.metadata.labels must not declare an empty id"
    );
    assert_eq!(
        rejection(document(json!({ "metadata": { "categories": { "a": "x" } } }))).to_string(),
        "$.metadata.categories.a must be an object"
    );
    assert_eq!(
        rejection(document(
            json!({ "metadata": { "sortKeys": { "a": { "title": "A", "order": "up" } } } })
        ))
        .to_string(),
        "$.metadata.sortKeys.a.order must be \"asc\" or \"desc\""
    );
}

#[test]
fn rejects_plugin_identity_author_and_version_list_violations() {
    assert_eq!(
        entry_rejection(json!({ "name": "Bad Name" })),
        "$.plugins[0].name must be a valid npm package name"
    );
    assert_eq!(
        entry_rejection(json!({ "name": format!("a{}", "b".repeat(214)) })),
        "$.plugins[0].name must be a valid npm package name"
    );
    assert_eq!(
        entry_rejection(json!({ "author": " " })),
        "$.plugins[0].author must be a string with a non-whitespace character"
    );
    assert_eq!(
        entry_rejection(json!({ "author": { "name": "Jane" } })),
        "$.plugins[0].author must be a string with a non-whitespace character"
    );
    assert_eq!(
        entry_rejection(json!({ "homepage": "javascript:alert(1)" })),
        "$.plugins[0].homepage must use https: or http:"
    );
    assert_eq!(
        entry_rejection(json!({ "homepage": "docs/foo" })),
        "$.plugins[0].homepage must be an absolute URL"
    );
    assert_eq!(
        entry_rejection(json!({ "homepage": 3 })),
        "$.plugins[0].homepage must be a string with a non-whitespace character"
    );
    // An intranet homepage is legitimate: the format restricts the scheme, not the host.
    assert_eq!(
        parse_market_index(
            &document(json!({ "plugins": [entry(json!({ "homepage": "http://intranet.example/foo" }))] })),
            INDEX_URL
        )
        .expect("an intranet homepage is accepted")
        .plugins[0]
            .homepage
            .as_deref(),
        Some("http://intranet.example/foo")
    );
    assert_eq!(
        parse_market_index(
            &document(json!({ "plugins": [entry(json!({ "repository": { "url": "http://git.intranet/o/r" } }))] })),
            INDEX_URL
        )
        .expect("an intranet repository is accepted")
        .plugins[0]
            .repository,
        Some(plugin_market_gap::MarketRepository {
            url: "http://git.intranet/o/r".to_owned(),
            directory: None,
        })
    );
    assert_eq!(
        entry_rejection(json!({ "repository": "https://github.com/o/r" })),
        "$.plugins[0].repository must be an object"
    );
    assert_eq!(
        entry_rejection(json!({ "repository": { "url": "git+ssh://git@github.com/o/r.git" } })),
        "$.plugins[0].repository.url must use https: or http:"
    );
    assert_eq!(
        entry_rejection(json!({ "repository": { "url": "https://github.com/o/r#main" } })),
        "$.plugins[0].repository.url must not contain a query or fragment"
    );
    assert_eq!(
        entry_rejection(json!({ "repository": { "url": "https://github.com/o/r", "directory": "../x" } })),
        "$.plugins[0].repository.directory must be a relative directory of letters, digits, and ._@+- segments without . or .."
    );
    assert_eq!(
        entry_rejection(json!({ "versions": [] })),
        "$.plugins[0].versions must be a non-empty array"
    );
    assert_eq!(
        entry_rejection(json!({ "versions": npm_version(json!({})) })),
        "$.plugins[0].versions must be a non-empty array"
    );
    assert_eq!(
        entry_rejection(json!({ "versions": [npm_version(json!({})), npm_version(json!({ "publishTimestamp": 5 }))] })),
        "$.plugins[0].versions[1].version duplicates 1.2.3"
    );
    assert_eq!(
        rejection(document(json!({ "plugins": [entry(json!({})), entry(json!({}))] }))).to_string(),
        "$.plugins[1].name duplicates @scope/dsh-foo"
    );
}

#[test]
fn rejects_version_identity_time_and_engine_violations() {
    assert_eq!(
        entry_rejection(json!({ "versions": ["1.2.3"] })),
        "$.plugins[0].versions[0] must be an object"
    );
    for version in ["v1.2.3", "^1.2.3", "1.2.3beta", "01.2.3"] {
        assert_eq!(
            version_rejection(json!({ "version": version })),
            "$.plugins[0].versions[0].version must be an exact SemVer version",
            "{version}"
        );
    }
    for publish_timestamp in [json!("1"), json!(1.5), json!(-1), json!(1e11)] {
        assert_eq!(
            version_rejection(json!({ "publishTimestamp": publish_timestamp })),
            "$.plugins[0].versions[0].publishTimestamp must be an integer number of Unix seconds in [0, 10^11)",
            "{publish_timestamp}"
        );
    }
    assert_eq!(
        version_rejection(json!({ "engines": ">=1" })),
        "$.plugins[0].versions[0].engines must be an object"
    );
    for dsh in [json!(""), json!(1)] {
        assert_eq!(
            version_rejection(json!({ "engines": { "dsh": dsh } })),
            "$.plugins[0].versions[0].engines.dsh must be a string with a non-whitespace character"
        );
    }
    assert_eq!(
        version_rejection(json!({ "engines": { "dsh": "workspace:*" } })),
        "$.plugins[0].versions[0].engines.dsh must not be a workspace: range"
    );
    assert_eq!(
        version_rejection(json!({ "engines": { "dsh": "broken" } })),
        "$.plugins[0].versions[0].engines.dsh must be a SemVer range"
    );
}

#[test]
fn rejects_malformed_sources() {
    let at = "$.plugins[0].versions[0].source";
    let npm = |fields: serde_json::Value| version_rejection(json!({ "source": merge_source(json!({ "type": "npm" }), fields) }));
    assert_eq!(
        version_rejection(json!({ "source": { "type": "tarball" } })),
        format!("{at}.type must be \"npm\" or \"git\"")
    );
    assert_eq!(npm(json!({ "registry": "not a url" })), format!("{at}.registry must be an absolute URL"));
    assert_eq!(
        npm(json!({ "registry": "ftp://registry.example/" })),
        format!("{at}.registry must use https: or http:")
    );
    assert_eq!(
        npm(json!({ "registry": "https://user:pw@registry.example/" })),
        format!("{at}.registry must not contain credentials")
    );
    assert_eq!(
        npm(json!({ "registry": "https://registry.example/?a=1" })),
        format!("{at}.registry must not contain a query or fragment")
    );
    assert_eq!(
        npm(json!({ "registry": "https://registry.example/?" })),
        format!("{at}.registry must not contain a query or fragment")
    );
    let git = |fields: serde_json::Value| {
        version_rejection(json!({
            "source": merge_source(
                json!({ "type": "git", "url": "https://github.com/o/r", "commit": COMMIT }),
                fields
            )
        }))
    };
    assert_eq!(
        git(json!({ "commit": COMMIT.to_uppercase() })),
        format!("{at}.commit must be a lowercase 40-hex-digit commit SHA")
    );
    assert_eq!(
        git(json!({ "commit": &COMMIT[..7] })),
        format!("{at}.commit must be a lowercase 40-hex-digit commit SHA")
    );
    assert_eq!(git(json!({ "url": "http://github.com/o/r" })), format!("{at}.url must use https:"));
    assert_eq!(
        git(json!({ "url": "https://GitHub.com/o/r" })),
        format!("{at}.url must be a canonical URL")
    );
    assert_eq!(
        git(json!({ "url": "https://github.com/o/r#main" })),
        format!("{at}.url must not contain a query or fragment")
    );
    assert_eq!(
        git(json!({ "url": "https://github.com/o/r#" })),
        format!("{at}.url must not contain a query or fragment")
    );
    assert_eq!(
        git(json!({ "url": "https://github.com/o/r?" })),
        format!("{at}.url must not contain a query or fragment")
    );
    assert_eq!(
        git(json!({ "url": "https://github.com/o" })),
        format!("{at}.url must name a repository with at least two path segments")
    );
    assert_eq!(
        git(json!({ "url": "https://github.com/o/r/" })),
        format!("{at}.url must name a repository with at least two path segments")
    );
    assert_eq!(
        git(json!({ "url": "https://example.com/o/r.tgz" })),
        format!("{at}.url must name a repository, not a tarball")
    );
    for path in ["../x", "a/./b", "a//b", "a&b", "a b"] {
        assert_eq!(
            git(json!({ "path": path })),
            format!("{at}.path must be a relative directory of letters, digits, and ._@+- segments without . or .."),
            "{path}"
        );
    }
}

/// Merge `fields` into a source object, for the source cases.
fn merge_source(mut base: serde_json::Value, fields: serde_json::Value) -> serde_json::Value {
    merge(&mut base, fields);
    base
}

#[test]
fn rejects_malformed_icons_and_market_values() {
    let data_rule = "$.plugins[0].icon must be a base64 SVG, PNG, JPEG, or WebP data URL";
    assert_eq!(entry_rejection(json!({ "icon": "data:image/gif;base64,R0lG" })), data_rule);
    assert_eq!(entry_rejection(json!({ "icon": "data:image/png;base64,abc" })), data_rule);
    assert_eq!(entry_rejection(json!({ "icon": "data:image/png;base64," })), data_rule);
    // 43,691 base64 characters plus padding is exactly 32 KiB decoded; one more character is not.
    let largest = format!("data:image/png;base64,{}=", "A".repeat(43_691));
    assert_eq!(
        parse_market_index(
            &document(json!({ "plugins": [entry(json!({ "icon": largest }))] })),
            INDEX_URL
        )
        .expect("the largest accepted icon is accepted")
        .plugins[0]
            .icon
            .as_deref(),
        Some(largest.as_str())
    );
    assert_eq!(
        entry_rejection(json!({ "icon": format!("data:image/png;base64,{}", "A".repeat(43_692)) })),
        format!(
            "$.plugins[0].icon must not exceed {} KiB of decoded bytes",
            MAX_INLINE_ICON_BYTES / 1024
        )
    );
    assert_eq!(
        entry_rejection(json!({ "icon": "http://cdn.example/a.png" })),
        "$.plugins[0].icon must resolve to https or the index origin"
    );
    assert_eq!(
        entry_rejection(json!({ "icon": "javascript:alert(1)" })),
        "$.plugins[0].icon must resolve to https or the index origin"
    );
    assert_eq!(
        entry_rejection(json!({ "metadata": { "category": "missing" } })),
        "$.plugins[0].metadata.category references undeclared id \"missing\""
    );
    assert_eq!(
        entry_rejection(json!({ "metadata": { "labels": "official" } })),
        "$.plugins[0].metadata.labels must be an array"
    );
    assert_eq!(
        entry_rejection(json!({ "metadata": { "labels": ["official", "official"] } })),
        "$.plugins[0].metadata.labels must not repeat a label"
    );
    assert_eq!(
        entry_rejection(json!({ "metadata": { "labels": ["constructor"] } })),
        "$.plugins[0].metadata.labels[0] references undeclared id \"constructor\""
    );
    assert_eq!(
        entry_rejection(json!({ "metadata": { "sortKeys": { "rating": 1 } } })),
        "$.plugins[0].metadata.sortKeys.rating references undeclared id \"rating\""
    );
}

#[test]
fn refuses_a_relative_index_url_with_its_own_error() {
    assert!(parse_market_index(&json!({}), "index.json").is_err());
}

#[test]
fn keeps_ids_such_as_proto_as_ordinary_keys() {
    let raw = r#"{"format":1,"title":"T","metadata":{"labels":{"__proto__":{"title":"Proto"}},"sortKeys":{"__proto__":{"title":"P","order":"asc"}}},"plugins":[{"name":"a","versions":[{"version":"1.0.0","publishTimestamp":0,"source":{"type":"npm"}}],"metadata":{"labels":["__proto__"],"sortKeys":{"__proto__":3}}}]}"#;
    let value: serde_json::Value = serde_json::from_str(raw).expect("valid JSON");
    let index = parse_market_index(&value, INDEX_URL).expect("the index validates");
    assert_eq!(index.plugins[0].metadata.labels, ["__proto__"]);
    assert_eq!(index.plugins[0].metadata.sort_keys.get("__proto__"), Some(&3.0));
}

#[test]
fn rejects_a_file_origin_icon_resolved_against_a_file_index() {
    let error = parse_market_index(
        &document(json!({ "plugins": [entry(json!({ "icon": "a.png" }))] })),
        "file:///srv/index.json",
    )
    .expect_err("a file index names no origin to resolve against");
    assert!(error.to_string().contains("must be an absolute URL"), "{error}");
}

// ============================================================================================
// Index bodies — upstream `parse.spec.ts`
// ============================================================================================

#[test]
fn validates_a_body_and_resolves_relative_icons_against_the_index_url() {
    let body = br#"{"format":1,"title":"M","plugins":[{"name":"p","icon":"p.svg","versions":[{"version":"1.0.0","publishTimestamp":1,"source":{"type":"npm"}}]}]}"#;
    let IndexParseResult::Ok(index) = parse_index_body(body, INDEX_URL) else {
        panic!("the body must validate");
    };
    assert_eq!(
        index.plugins[0].icon.as_deref(),
        Some("https://market.example/p.svg")
    );
}

#[test]
fn reports_malformed_utf8_json_and_format_violations() {
    assert_eq!(
        parse_index_body(&[0xff, 0xfe], INDEX_URL),
        IndexParseResult::Invalid {
            path: "$".to_owned(),
            reason: "must be UTF-8".to_owned(),
            message: "must be UTF-8".to_owned(),
        }
    );
    let IndexParseResult::Invalid { path, reason, .. } = parse_index_body(b"{", INDEX_URL) else {
        panic!("malformed JSON must be reported");
    };
    assert_eq!(path, "$");
    assert!(reason.starts_with("must be JSON: "), "{reason}");
    assert_eq!(
        parse_index_body(br#"{"format":2}"#, INDEX_URL),
        IndexParseResult::Invalid {
            path: "$.format".to_owned(),
            reason: "must be 1".to_owned(),
            message: "$.format must be 1".to_owned(),
        }
    );
}

// ============================================================================================
// Index-source URLs — upstream `index-url.ts`
// ============================================================================================

#[test]
fn normalizes_index_urls_to_their_canonical_form() {
    assert_eq!(
        normalize_index_url("  https://market.example/index.json  "),
        Ok("https://market.example/index.json".to_owned())
    );
    // Scheme and host case, the default port, and the root path all canonicalize.
    assert_eq!(
        normalize_index_url("HTTPS://Market.Example:443"),
        Ok("https://market.example/".to_owned())
    );
    assert_eq!(
        normalize_index_url("http://market.example:80/a/./b/../c"),
        Ok("http://market.example/a/c".to_owned())
    );
    // A non-default port is kept.
    assert_eq!(
        normalize_index_url("http://127.0.0.1:8080/index.json"),
        Ok("http://127.0.0.1:8080/index.json".to_owned())
    );
    // A query is allowed: it is part of what the source names.
    assert_eq!(
        normalize_index_url("https://market.example/i.json?v=1"),
        Ok("https://market.example/i.json?v=1".to_owned())
    );
}

#[test]
fn refuses_a_non_absolute_url() {
    for text in ["index.json", "", "   ", "//market.example/i.json"] {
        let error = normalize_index_url(text).expect_err("the URL is not absolute");
        assert_eq!(error.reason, IndexUrlReason::NotAbsolute, "{text:?}");
        assert_eq!(error.reason.label(), "not-absolute");
    }
}

#[test]
fn refuses_a_scheme_that_is_not_http_or_https() {
    for text in ["ftp://market.example/i.json", "file:///srv/i.json"] {
        let error = normalize_index_url(text).expect_err("the scheme is refused");
        // A non-http(s) scheme does not parse as a http(s) URL here, so it is refused for that.
        assert!(
            matches!(
                error.reason,
                IndexUrlReason::Scheme | IndexUrlReason::NotAbsolute
            ),
            "{text:?}: {:?}",
            error.reason
        );
    }
}

#[test]
fn refuses_credentials_and_fragments() {
    for text in [
        "https://user:secret@market.example/i.json",
        "https://user@market.example/i.json",
        "https://market.example/i.json#section",
        "https://market.example/i.json#",
    ] {
        let error = normalize_index_url(text).expect_err("credentials or a fragment are refused");
        assert_eq!(error.reason, IndexUrlReason::CredentialsOrFragment, "{text:?}");
        assert_eq!(error.reason.label(), "credentials-or-fragment");
        // The message names the rule, and the details keep the URL as written.
        assert!(error.message.contains("no credentials or fragment"), "{error}");
        assert_eq!(error.url, text);
    }
}

#[test]
fn a_normalized_url_is_its_own_canonical_form() {
    // Idempotence is what `sources()` relies on: it compares `normalize(url) == url` to report a
    // source as non-canonical instead of refusing it.
    for text in [
        "https://market.example/index.json",
        "http://127.0.0.1:8080/a/b.json",
        "https://market.example/i.json?v=1",
    ] {
        let once = normalize_index_url(text).expect("the URL is valid");
        let twice = normalize_index_url(&once).expect("its own form is valid");
        assert_eq!(once, twice, "{text}");
    }
}

#[test]
fn resolves_relative_references_the_way_the_browser_does() {
    let base = Url::parse(INDEX_URL).expect("the index URL parses");
    let resolve = |reference: &str| {
        plugin_market_gap::resolve_url(reference, &base).map(|url| url.href())
    };
    assert_eq!(resolve("icons/foo.svg").as_deref(), Some("https://market.example/dsh/icons/foo.svg"));
    assert_eq!(resolve("/b.png").as_deref(), Some("https://market.example/b.png"));
    assert_eq!(resolve("../c.png").as_deref(), Some("https://market.example/c.png"));
    assert_eq!(resolve("//cdn.example/d.png").as_deref(), Some("https://cdn.example/d.png"));
    assert_eq!(resolve("https://cdn.example/e.png").as_deref(), Some("https://cdn.example/e.png"));
    // A reference this subset cannot resolve names no URL rather than a wrong one.
    assert_eq!(resolve("https://["), None);
}

// ============================================================================================
// Install arguments — upstream `MarketInstallTarget`
// ============================================================================================

/// An exact version built for the install-target cases.
fn version(version: &str, source: MarketSource) -> MarketVersion {
    MarketVersion {
        version: version.to_owned(),
        publish_timestamp: 0,
        source,
        engines: plugin_market_gap::MarketEngines::default(),
    }
}

#[test]
fn pins_npm_versions_to_their_exact_version_and_registry() {
    assert_eq!(
        market_install_target("dsh-foo", &version("1.2.3", MarketSource::Npm { registry: None })),
        plugin_market_gap::MarketInstallTarget {
            spec: "dsh-foo@1.2.3".to_owned(),
            registry: None,
        }
    );
    assert_eq!(
        market_install_target(
            "@s/dsh-foo",
            &version(
                "1.2.3",
                MarketSource::Npm {
                    registry: Some("https://r.example/".to_owned())
                }
            )
        ),
        plugin_market_gap::MarketInstallTarget {
            spec: "@s/dsh-foo@1.2.3".to_owned(),
            registry: Some("https://r.example/".to_owned()),
        }
    );
}

#[test]
fn pins_git_versions_to_their_commit_and_package_directory() {
    let git = |url: &str, path: Option<&str>| {
        version(
            "1.0.0",
            MarketSource::Git {
                url: url.to_owned(),
                commit: COMMIT.to_owned(),
                path: path.map(str::to_owned),
            },
        )
    };
    assert_eq!(
        market_install_target("a", &git("https://github.com/o/r", None)).spec,
        format!("git+https://github.com/o/r.git#{COMMIT}")
    );
    assert_eq!(
        market_install_target("a", &git("https://gitlab.example/g/s/r.git", Some("packages/a"))).spec,
        format!("git+https://gitlab.example/g/s/r.git#{COMMIT}&path:/packages/a")
    );
}

// ============================================================================================
// Range syntax — the check `engines.dsh` makes
// ============================================================================================

#[test]
fn accepts_the_range_syntax_node_semver_accepts() {
    for range in [
        "", "*", "x", ">=0.2.0", "^1.2.3", "~1.2", "1.2.x", "1", "1.2", "1.2.3", "1.2.3 - 2.3.4",
        ">=1.2.3 <2", ">1.2.3 || <0.1.0", "=1.2.3", ">= 1.2.3", "~>1.2.3", "1.2.3-beta.1",
        "1.2.3+build.5", ">=1.2.3-rc.1 <2.0.0",
    ] {
        assert!(is_semver_range(range), "{range:?} must be a range");
    }
}

#[test]
fn refuses_the_range_syntax_node_semver_refuses() {
    for range in ["broken", "latest", "1.2.3beta", "^", ">= 1.2.3 <", "1.2.3 - ", "01.2.3", "1.x.3"] {
        assert!(!is_semver_range(range), "{range:?} must not be a range");
    }
}

// ============================================================================================
// The properties the decisions rest on
// ============================================================================================

#[test]
fn a_fuzzy_match_never_exceeds_two_edits() {
    // `maxFuzzy: 2` is what makes `dsx` match nothing: the term is shorter than four characters, so
    // its radius is zero, and `dsh` is two substitutions away only at a radius the setting forbids.
    assert_eq!(searched(&text("dsx")), Vec::<String>::new());
    // A six-character term gets a radius of one (6 * 0.2 rounded), which is why `revew` matches.
    assert_eq!(searched(&text("revew")).len(), 3);
    // A very long term's radius is capped at two, not at a fifth of its length.
    let index = parsed(json!({
        "format": 1,
        "title": "Long",
        "plugins": [
            entry(json!({ "name": "p", "metadata": { "keywords": ["internationalization"] } }))
        ]
    }));
    assert_eq!(
        names(&index, &text("internaxionalizatiox")),
        Vec::<String>::new(),
        "three edits must not match"
    );
    assert_eq!(
        names(&index, &text("internaxionalization")),
        ["p"],
        "two edits must match"
    );
}

#[test]
fn every_query_term_must_match_some_field() {
    // `combineWith: 'AND'` across terms: `forecast` is in one plugin and `lint` in another, so the
    // conjunction is empty even though each term matches something.
    assert_eq!(searched(&text("forecast lint")), Vec::<String>::new());
    assert_eq!(searched(&text("forecast kim")), ["dsh-weather"]);
}

#[test]
fn searching_never_returns_a_plugin_twice() {
    let index = searchable();
    let search = SearchIndex::new(&index);
    for query in ["review", "revew", "代码审查", "e", "r"] {
        let page = search.select(&text(query)).expect("a valid page");
        let distinct: std::collections::BTreeSet<usize> = page.positions.iter().copied().collect();
        assert_eq!(distinct.len(), page.positions.len(), "{query}");
    }
}

#[test]
fn filtering_never_adds_a_plugin_the_text_did_not_match() {
    let index = searchable();
    let search = SearchIndex::new(&index);
    let unfiltered = search.select(&text("review")).expect("a valid page");
    let filtered = search
        .select(&MarketQuery {
            text: Some("review".to_owned()),
            category: Some("work".to_owned()),
            ..MarketQuery::default()
        })
        .expect("a valid page");
    let allowed: std::collections::BTreeSet<usize> = unfiltered.positions.iter().copied().collect();
    assert!(filtered.positions.iter().all(|position| allowed.contains(position)));
    assert!(filtered.total <= unfiltered.total);
}

#[test]
fn an_empty_index_answers_every_query_with_an_empty_page() {
    let index = parsed(json!({ "format": 1, "title": "Empty", "plugins": [] }));
    let search = SearchIndex::new(&index);
    for query in [all(), text("anything"), text(" -- ")] {
        let page = search.select(&query).expect("a valid page");
        assert!(page.positions.is_empty());
        assert_eq!(page.total, 0);
        assert_eq!(page.next_offset, None);
    }
}

#[test]
fn a_query_that_matches_nothing_still_reports_the_total_of_zero() {
    let page = SearchIndex::new(&searchable())
        .select(&text("zzzzzzzz"))
        .expect("a valid page");
    assert_eq!(page.total, 0);
    assert_eq!(page.next_offset, None);
    assert!(page.positions.is_empty());
}
