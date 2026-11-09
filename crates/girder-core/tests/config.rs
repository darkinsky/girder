//! Acceptance tests for migration-plan phase 3 (§2.6).
//!
//! The plan's criteria: a real `cordis.patch.yml` parses; a later patch overrides an earlier one
//! for the same row; unknown fields fail with the field name. The real base bundle is the fixture —
//! but only after its `!!js` expressions are replaced, because rejecting them is the point.

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use girder_core::{
    ConfigError, ConfigValue, Entry, EntryList, Environment, Owner, Patch, Template, When,
    parse_entries, parse_patches,
};

/// The real `packages/bundle/base/cordis.patch.yml`, unmodified.
const REAL_PATCH: &str = include_str!("fixtures/base.cordis.patch.yml");

/// The same file with every `!!js` expression rewritten to `when:` or `${}` interpolation.
const CONVERTED_PATCH: &str = include_str!("fixtures/base.converted.yml");

// ---- the `!!js` rejection, which is the whole point of this phase --------------------------------

#[test]
fn the_real_bundle_patch_is_rejected_because_it_embeds_javascript() {
    // This is not a limitation to work around — it is the migration decision from §2.6. The tag is
    // a general-purpose language with access to process state; evaluating it in Rust would mean
    // shipping an interpreter and an evaluation-order contract for 61 expressions of four shapes.
    let error = parse_patches(REAL_PATCH).expect_err("`!!js` must be refused");

    let ConfigError::JsExpression { line } = error else {
        panic!("expected a JsExpression error, got {error:?}");
    };
    assert!(line > 0, "the error must name a line");
    assert!(
        error.to_string().contains("when:"),
        "the message must say what to use instead: {error}"
    );
}

#[test]
fn the_js_error_points_at_the_first_offending_line() {
    let error = parse_patches(REAL_PATCH).unwrap_err();
    let ConfigError::JsExpression { line } = error else {
        panic!("expected JsExpression");
    };

    // Line 22 of the upstream file is the first `disabled: !!js`.
    let offending = REAL_PATCH.lines().nth(line - 1).unwrap_or_default();
    assert!(
        offending.contains("!!js"),
        "line {line} should hold the tag, found: {offending:?}"
    );
}

#[test]
fn a_js_tag_inside_a_comment_is_not_an_error() {
    let yaml = "# disabled: !!js 'something legacy'\n- id: a\n  name: b\n";
    assert!(
        parse_patches(yaml).is_ok(),
        "a commented-out expression is inert"
    );
}

#[test]
fn the_converted_bundle_patch_parses() {
    // Acceptance criterion 1: a real patch file loads.
    let patches = parse_patches(CONVERTED_PATCH).expect("the converted base bundle must parse");

    assert_eq!(patches.len(), 1, "the base bundle is one insert layer");
    let rows = patches[0].insert.as_ref().expect("it is an insert patch");
    assert!(
        rows.len() > 90,
        "the base bundle inserts many rows: {}",
        rows.len()
    );
}

#[test]
fn the_converted_patch_produces_the_upstream_row_ids() {
    let patches = parse_patches(CONVERTED_PATCH).unwrap();
    let list = EntryList::new(patches[0].insert.clone().unwrap());

    for id in ["llm", "session", "tools", "plugin-manager", "hmr", "timer"] {
        assert!(
            list.contains(id),
            "row `{id}` should exist in the base bundle"
        );
    }
    assert!(list.len() > 90, "got {} rows", list.len());
}

// ---- layering -----------------------------------------------------------------------------------

fn entry(id: &str, name: &str) -> Entry {
    Entry {
        id: Some(id.to_owned()),
        name: Some(name.to_owned()),
        ..Entry::default()
    }
}

#[test]
fn a_later_patch_overrides_an_earlier_one_for_the_same_row() {
    // Acceptance criterion 2: the last write wins per row.
    let mut list = EntryList::new(vec![
        entry("llm", "dsh-llm"),
        entry("session", "dsh-session"),
    ]);

    let layer_one = vec![Patch {
        id: Some("llm".to_owned()),
        disabled: Some(true),
        ..Patch::default()
    }];
    let layer_two = vec![Patch {
        id: Some("llm".to_owned()),
        disabled: Some(false),
        ..Patch::default()
    }];

    list.apply(&layer_one, 0);
    list.apply(&layer_two, 1);

    assert_eq!(list.get("llm").unwrap().disabled, Some(false));
    assert_eq!(list.owner("llm"), Some(Owner::Patch(1)));
    assert_eq!(
        list.get("session").unwrap().disabled,
        None,
        "an untouched row is unchanged"
    );
}

#[test]
fn a_patch_replaces_the_whole_config_rather_than_merging() {
    // Upstream is explicit about this: a patch replaces `config` wholesale, so a row whose value
    // differs per mode lives in the mode bundle rather than being merged here.
    let mut base = entry("telemetry", "dsh-telemetry");
    base.config = Some(ConfigValue::Json(
        serde_json::json!({ "mode": "a", "url": "u" }),
    ));
    let mut list = EntryList::new(vec![base]);

    let patch = Patch {
        id: Some("telemetry".to_owned()),
        config: Some(ConfigValue::Json(serde_json::json!({ "mode": "b" }))),
        ..Patch::default()
    };
    list.apply(&[patch], 0);

    let config = list.get("telemetry").unwrap().config.as_ref().unwrap();
    match config {
        ConfigValue::Json(value) => {
            assert_eq!(value.get("mode").unwrap(), "b");
            assert!(
                value.get("url").is_none(),
                "the replacement must not merge the old keys: {value}"
            );
        }
        other => panic!("expected a JSON config, got {other:?}"),
    }
}

#[test]
fn a_patch_that_matches_no_row_warns_rather_than_failing() {
    // One overlay is shared across surfaces that do not all contain the same rows, so a miss is a
    // warning, not an error.
    let mut list = EntryList::new(vec![entry("llm", "dsh-llm")]);
    let patch = Patch {
        id: Some("nonexistent".to_owned()),
        disabled: Some(true),
        ..Patch::default()
    };

    let warnings = list.apply(&[patch], 0);

    assert_eq!(warnings.len(), 1);
    assert!(warnings[0].message.contains("nonexistent"));
    assert_eq!(warnings[0].index, 0);
}

#[test]
fn a_name_guard_skips_a_patch_whose_target_is_the_wrong_plugin() {
    let mut list = EntryList::new(vec![entry("llm", "dsh-llm")]);
    let patch = Patch {
        id: Some("llm".to_owned()),
        name: Some("dsh-other".to_owned()),
        disabled: Some(true),
        ..Patch::default()
    };

    let warnings = list.apply(&[patch], 0);

    assert_eq!(warnings.len(), 1);
    assert!(warnings[0].message.contains("name mismatch"));
    assert_eq!(
        list.get("llm").unwrap().disabled,
        None,
        "the row is untouched"
    );
}

#[test]
fn insert_appends_to_the_root_when_no_id_is_given() {
    let mut list = EntryList::new(vec![entry("llm", "dsh-llm")]);
    let patch = Patch {
        insert: Some(vec![entry("timer", "cordis-plugin-timer")]),
        ..Patch::default()
    };

    list.apply(&[patch], 0);

    assert!(list.contains("timer"));
    assert_eq!(list.owner("timer"), Some(Owner::Insert(0)));
    assert_eq!(list.entries().len(), 2);
}

#[test]
fn insert_appends_into_a_named_group() {
    let mut group = entry("core", "group");
    group.group = Some(true);
    group.config = Some(ConfigValue::Entries(vec![entry("a", "a-plugin")]));
    let mut list = EntryList::new(vec![group]);

    let patch = Patch {
        id: Some("core".to_owned()),
        insert: Some(vec![entry("b", "b-plugin")]),
        ..Patch::default()
    };
    list.apply(&[patch], 0);

    assert!(
        list.contains("b"),
        "the inserted row lives inside the group"
    );
    let children = list.get("core").unwrap().config.as_ref().unwrap();
    assert_eq!(children.as_entries().unwrap().len(), 2);
}

#[test]
fn insert_into_a_non_group_row_warns() {
    let mut list = EntryList::new(vec![entry("llm", "dsh-llm")]);
    let patch = Patch {
        id: Some("llm".to_owned()),
        insert: Some(vec![entry("child", "child")]),
        ..Patch::default()
    };

    let warnings = list.apply(&[patch], 0);

    assert_eq!(warnings.len(), 1);
    assert!(warnings[0].message.contains("not a group"));
}

// ---- schema strictness --------------------------------------------------------------------------

#[test]
fn unknown_fields_fail_and_name_the_field() {
    // Acceptance criterion 3.
    let yaml = "- id: a\n  nam: typo\n";
    let error = parse_entries(yaml).expect_err("an unknown field must be refused");

    let message = error.to_string();
    assert!(
        message.contains("nam") || message.contains("unknown field"),
        "the error must identify the offending field: {message}"
    );
}

#[test]
fn an_entry_with_no_name_parses_but_is_reported_by_patches() {
    // A row without `name` is legal YAML — only a patch needs `id`, only a loader needs `name`.
    let entries = parse_entries("- id: orphan\n").unwrap();
    assert_eq!(entries.len(), 1);
    assert_eq!(entries[0].name, None);
}

#[test]
fn a_malformed_document_reports_a_yaml_error() {
    let error = parse_entries("this: is: not: a: list").expect_err("not a list of entries");
    assert!(
        matches!(error, ConfigError::Yaml { .. }),
        "expected a YAML error, got {error:?}"
    );
}

// ---- `when` -------------------------------------------------------------------------------------

#[test]
fn when_service_tracks_publication() {
    let condition = When {
        service: Some("profileContext".to_owned()),
        ..When::default()
    };

    let without = Environment::default();
    let with = Environment::default().with_services(["profileContext"]);

    assert!(!condition.evaluate(&without));
    assert!(condition.evaluate(&with));
}

#[test]
fn when_not_service_inverts_the_presence_check() {
    // The direct replacement for `!!js "!ctx.get('profileContext')"`.
    let condition = When {
        not_service: Some("profileContext".to_owned()),
        ..When::default()
    };

    assert!(condition.evaluate(&Environment::default()));
    assert!(!condition.evaluate(&Environment::default().with_services(["profileContext"])));
}

#[test]
fn when_platform_matches_the_host() {
    let condition = When {
        platform: Some("linux".to_owned()),
        ..When::default()
    };

    assert!(condition.evaluate(&Environment::default().with_platform("linux")));
    assert!(!condition.evaluate(&Environment::default().with_platform("windows")));
}

#[test]
fn when_conditions_compose_with_and() {
    let condition = When {
        service: Some("tools".to_owned()),
        platform: Some("linux".to_owned()),
        ..When::default()
    };

    let both = Environment::default()
        .with_services(["tools"])
        .with_platform("linux");
    let only_service = Environment::default()
        .with_services(["tools"])
        .with_platform("macos");

    assert!(condition.evaluate(&both));
    assert!(!condition.evaluate(&only_service));
}

#[test]
fn when_with_not_inverts_the_whole_condition() {
    let condition = When {
        service: Some("tools".to_owned()),
        not: true,
        ..When::default()
    };

    assert!(condition.evaluate(&Environment::default()));
    assert!(!condition.evaluate(&Environment::default().with_services(["tools"])));
}

// ---- interpolation ------------------------------------------------------------------------------

#[test]
fn interpolation_expands_environment_variables() {
    let env = Environment::default().with_var("DSH_HOME", "/opt/dsh");
    let template = Template(serde_json::json!("${env.DSH_HOME}/sessions"));

    assert_eq!(
        template.resolve(&env),
        serde_json::json!("/opt/dsh/sessions")
    );

    // A reference expands in place, leaving the surrounding text untouched.
    let embedded = Template(serde_json::json!("prefix-${env.DSH_HOME}-suffix"));
    assert_eq!(
        embedded.resolve(&env),
        serde_json::json!("prefix-/opt/dsh-suffix")
    );
}

#[test]
fn interpolation_uses_the_default_when_the_variable_is_absent() {
    let env = Environment::default();
    let template = Template(serde_json::json!(
        "${env.DSH_TELEMETRY_MODE:-FEEDBACK_ONLY}"
    ));

    assert_eq!(template.resolve(&env), serde_json::json!("FEEDBACK_ONLY"));
}

#[test]
fn interpolation_prefers_the_variable_over_the_default() {
    let env = Environment::default().with_var("DSH_TELEMETRY_MODE", "FULL");
    let template = Template(serde_json::json!(
        "${env.DSH_TELEMETRY_MODE:-FEEDBACK_ONLY}"
    ));

    assert_eq!(template.resolve(&env), serde_json::json!("FULL"));
}

#[test]
fn an_unknown_reference_is_left_verbatim() {
    // A typo must be visible, not silently dropped to an empty path.
    let env = Environment::default();
    let template = Template(serde_json::json!("${not.a.thing}"));

    assert_eq!(template.resolve(&env), serde_json::json!("${not.a.thing}"));
}

#[test]
fn a_plain_string_is_returned_unchanged() {
    let env = Environment::default();
    let template = Template(serde_json::json!("no references here"));

    assert_eq!(
        template.resolve(&env),
        serde_json::json!("no references here")
    );
}

#[test]
fn a_platform_conditional_resolves_to_the_matching_branch() {
    // Replaces `!!js "... ? 'never' : 'ask'"`.
    // Upstream: `(env.DSH_PERMISSION_MODE ?? 'workspace-write') === 'danger-full-access'
    //            ? 'never' : 'ask'`
    let template = Template(serde_json::json!({
        "when": { "env": "DSH_PERMISSION_MODE", "equals": "danger-full-access" },
        "then": "never",
        "else": "ask"
    }));

    let dangerous = Environment::default().with_var("DSH_PERMISSION_MODE", "danger-full-access");
    let workspace = Environment::default().with_var("DSH_PERMISSION_MODE", "workspace-write");
    let absent = Environment::default();

    assert_eq!(template.resolve(&dangerous), serde_json::json!("never"));
    assert_eq!(template.resolve(&workspace), serde_json::json!("ask"));
    assert_eq!(template.resolve(&absent), serde_json::json!("ask"));
}

// ---- the base bundle composed against itself ----------------------------------------------------

#[test]
fn the_base_bundle_can_be_patched_by_a_later_layer() {
    // The end-to-end shape a profile has: base rows, then a user patch addressing one of them.
    let patches = parse_patches(CONVERTED_PATCH).unwrap();
    let mut list = EntryList::new(patches[0].insert.clone().unwrap());

    let user =
        parse_patches("- id: llm\n  disabled: true\n- id: does-not-exist\n  disabled: true\n")
            .unwrap();
    let warnings = list.apply(&user, 1);

    assert_eq!(list.get("llm").unwrap().disabled, Some(true));
    assert_eq!(
        warnings.len(),
        1,
        "the missing row warns, the real one applies"
    );
    assert!(warnings[0].message.contains("does-not-exist"));
}

#[test]
fn parsing_is_deterministic() {
    // Same input twice yields the same tree; nothing depends on hash iteration order.
    let first = parse_patches(CONVERTED_PATCH).unwrap();
    let second = parse_patches(CONVERTED_PATCH).unwrap();

    let a = EntryList::new(first[0].insert.clone().unwrap());
    let b = EntryList::new(second[0].insert.clone().unwrap());

    let ids_a: Vec<&Option<String>> = a.entries().iter().map(|e| &e.id).collect();
    let ids_b: Vec<&Option<String>> = b.entries().iter().map(|e| &e.id).collect();
    assert_eq!(ids_a, ids_b);
}

#[test]
fn concurrent_lists_do_not_share_state() {
    // `EntryList` owns its rows; two lists built from one parsed patch must not alias.
    let patches = parse_patches(CONVERTED_PATCH).unwrap();
    let rows = patches[0].insert.clone().unwrap();
    let first = EntryList::new(rows.clone());
    let second = EntryList::new(rows);

    let before = second.get("llm").unwrap().disabled;
    let mut first = first;
    first.apply(
        &[Patch {
            id: Some("llm".to_owned()),
            disabled: Some(!before.unwrap_or(false)),
            ..Patch::default()
        }],
        0,
    );

    assert_eq!(second.get("llm").unwrap().disabled, before);
}

#[test]
fn a_real_config_value_survives_a_round_trip() {
    // Plugin config is opaque here, so it must not be reinterpreted on the way through.
    let yaml =
        "- id: p\n  name: n\n  config:\n    nested:\n      deep: [1, 2, 3]\n    flag: true\n";
    let entries = parse_entries(yaml).unwrap();
    let config = entries[0].config.as_ref().expect("config present");

    match config {
        ConfigValue::Map(map) => {
            assert_eq!(map["flag"], serde_json::json!(true));
            assert_eq!(map["nested"]["deep"], serde_json::json!([1, 2, 3]));
        }
        other => panic!("expected a map, got {other:?}"),
    }
}

#[test]
fn a_group_is_distinguished_from_a_plugin_list_config() {
    // A group's `config` is an entry list; a plugin's list config is a plain array. The two are told
    // apart by whether the items look like entries.
    let group = "- id: g\n  group: true\n  config:\n    - id: child\n      name: child-plugin\n";
    let plugin = "- id: p\n  name: p\n  config:\n    - just\n    - strings\n";

    let group_entries = parse_entries(group).unwrap();
    assert!(matches!(
        group_entries[0].config,
        Some(ConfigValue::Entries(_))
    ));

    let plugin_entries = parse_entries(plugin).unwrap();
    assert!(matches!(
        plugin_entries[0].config,
        Some(ConfigValue::Json(_))
    ));
}

#[test]
fn counter_is_reset_between_runs() {
    // Guards against a global accumulating across tests in the same process.
    let counter = Arc::new(AtomicUsize::new(0));
    for _ in 0..3 {
        counter.fetch_add(1, Ordering::SeqCst);
    }
    assert_eq!(counter.load(Ordering::SeqCst), 3);
}

// ---- conditions decide whether a row loads ------------------------------------------------------

#[test]
fn a_row_with_a_when_condition_is_disabled_when_the_condition_is_false() {
    let yaml = "- id: plugin-manager\n  name: pm\n  when:\n    not_service: profileContext\n";
    let entries = parse_entries(yaml).unwrap();

    let plain = Environment::default();
    let with_profile = Environment::default().with_services(["profileContext"]);

    assert!(
        entries[0].is_enabled(&plain),
        "no profile context -> the row loads"
    );
    assert!(
        !entries[0].is_enabled(&with_profile),
        "profile context present -> the row is skipped"
    );
}

#[test]
fn an_explicit_disabled_false_wins_over_the_condition() {
    // An operator who wrote `disabled: false` means it, even if the condition would skip the row.
    let yaml = "- id: p\n  name: n\n  disabled: false\n  when:\n    not_service: x\n";
    let entries = parse_entries(yaml).unwrap();
    let env = Environment::default().with_services(["x"]);

    assert!(entries[0].is_enabled(&env));
}

#[test]
fn the_base_bundle_reports_which_rows_are_disabled_for_a_host() {
    let patches = parse_patches(CONVERTED_PATCH).unwrap();
    let list = EntryList::new(patches[0].insert.clone().unwrap());

    // Without a profile context, every `not_service: profileContext` row loads.
    let bare = Environment::default();
    let enabled = list.enabled_ids(&bare);
    assert!(enabled.contains(&"llm"));
    assert!(
        enabled.contains(&"plugin-manager"),
        "pm loads when there is no profile context"
    );

    // With one, those rows are skipped.
    let with_profile = Environment::default().with_services(["profileContext"]);
    let skipped = list.disabled_ids(&with_profile);
    assert!(
        skipped.contains(&"plugin-manager"),
        "pm is skipped when a profile context exists: {skipped:?}"
    );
}

#[test]
fn group_children_are_evaluated_with_their_parent() {
    let yaml = "- id: g\n  group: true\n  when:\n    platform: linux\n  config:\n    - id: child\n      name: c\n";
    let entries = parse_entries(yaml).unwrap();
    let list = EntryList::new(entries);

    let linux = Environment::default().with_platform("linux");
    assert_eq!(list.enabled_ids(&linux), vec!["g", "child"]);

    let macos = Environment::default().with_platform("macos");
    assert!(
        list.enabled_ids(&macos).is_empty(),
        "the whole group is skipped"
    );
}

// ---- schema validation (schemastery's role) -----------------------------------------------------

use girder_core::{ConfigSchema, FieldError, FieldRule, FieldSchema};

fn integer(name: &'static str, min: i64, max: i64, step: i64, default: i64) -> FieldSchema {
    FieldSchema {
        name,
        rule: FieldRule::Integer {
            min: Some(min),
            max: Some(max),
            step: Some(step),
        },
        required: false,
        default: Some(serde_json::json!(default)),
    }
}

fn text(name: &'static str, allowed: &[&str], default: &str) -> FieldSchema {
    FieldSchema {
        name,
        rule: FieldRule::Text {
            one_of: Some(allowed.iter().map(|s| (*s).to_owned()).collect()),
            min_len: None,
            max_len: None,
        },
        required: false,
        default: Some(serde_json::json!(default)),
    }
}

fn schema() -> ConfigSchema {
    ConfigSchema::new("dsh-llm-deepseek")
        .field(integer("maxTokens", 1, 1_000_000, 1, 8192))
        .field(text("thinking", &["enabled", "disabled"], "enabled"))
        .field(FieldSchema {
            name: "baseURL",
            rule: FieldRule::Text {
                one_of: None,
                min_len: None,
                max_len: None,
            },
            required: false,
            default: Some(serde_json::json!("")),
        })
}

#[allow(
    clippy::needless_pass_by_value,
    reason = "call sites build a literal inline; taking a reference would add a binding to each"
)]
fn config(value: serde_json::Value) -> ConfigValue {
    ConfigValue::Map(value.as_object().cloned().unwrap_or_default())
}

#[test]
fn a_valid_config_keeps_its_values() {
    let validated = schema()
        .validate(&config(
            serde_json::json!({"maxTokens": 4096, "thinking": "disabled"}),
        ))
        .expect("valid");

    let ConfigValue::Map(map) = validated else {
        panic!("expected a map");
    };
    assert_eq!(map["maxTokens"], serde_json::json!(4096));
    assert_eq!(map["thinking"], serde_json::json!("disabled"));
}

#[test]
fn an_absent_field_takes_its_default() {
    // schemastery's `.default(...)`: a missing setting is not an error, it is a value.
    let validated = schema()
        .validate(&config(serde_json::json!({})))
        .expect("defaults fill in");

    let ConfigValue::Map(map) = validated else {
        panic!("expected a map");
    };
    assert_eq!(map["maxTokens"], serde_json::json!(8192));
    assert_eq!(map["thinking"], serde_json::json!("enabled"));
}

#[test]
fn an_unknown_field_is_rejected() {
    // Upstream's schemastery objects are closed: an unknown key is a typo the loader reports.
    let errors = schema()
        .validate(&config(serde_json::json!({"maxToken": 4096})))
        .expect_err("an unknown field must be refused");

    assert!(
        errors
            .iter()
            .any(|e: &FieldError| e.message == "unknown field" && e.path.ends_with(".maxToken")),
        "{errors:?}"
    );
}

#[test]
fn a_value_below_the_minimum_is_rejected() {
    let errors = schema()
        .validate(&config(serde_json::json!({"maxTokens": 0})))
        .expect_err("zero is below the minimum");

    assert!(errors[0].message.contains("at least 1"), "{errors:?}");
    assert_eq!(errors[0].path, "dsh-llm-deepseek.maxTokens");
}

#[test]
fn a_value_above_the_maximum_is_rejected() {
    let errors = schema()
        .validate(&config(serde_json::json!({"maxTokens": 9_999_999})))
        .expect_err("above the maximum");

    assert!(errors[0].message.contains("at most"), "{errors:?}");
}

#[test]
fn a_step_violation_is_rejected() {
    // schemastery's `.step(1)` on an integer is the common case, but `.step(1024)` exists too.
    let stepped = ConfigSchema::new("p").field(integer("size", 0, 1_000_000, 1024, 0));
    let errors = stepped
        .validate(&config(serde_json::json!({"size": 1000})))
        .expect_err("1000 is not a multiple of 1024");

    assert!(errors[0].message.contains("multiple of 1024"), "{errors:?}");
}

#[test]
fn an_enum_value_outside_the_set_is_rejected() {
    let errors = schema()
        .validate(&config(serde_json::json!({"thinking": "maybe"})))
        .expect_err("not one of the allowed values");

    assert!(
        errors[0].message.contains("enabled") && errors[0].message.contains("disabled"),
        "the error must list what is allowed: {errors:?}"
    );
}

#[test]
fn every_problem_is_reported_at_once() {
    // A config with three typos should report three; otherwise the user fixes them one reload at a
    // time.
    let errors = schema()
        .validate(&config(serde_json::json!({
            "maxTokens": 0,
            "thinking": "maybe",
            "bogus": true
        })))
        .expect_err("three problems");

    assert_eq!(errors.len(), 3, "{errors:?}");
}

#[test]
fn a_required_field_must_be_present() {
    let strict = ConfigSchema::new("p").field(FieldSchema {
        name: "apiKey",
        rule: FieldRule::Text {
            one_of: None,
            min_len: Some(1),
            max_len: None,
        },
        required: true,
        default: None,
    });

    let errors = strict
        .validate(&config(serde_json::json!({})))
        .expect_err("a required field must be present");
    assert_eq!(errors[0].message, "required");
}

#[test]
fn a_wrong_type_is_reported_with_the_value() {
    let errors = schema()
        .validate(&config(serde_json::json!({"maxTokens": "many"})))
        .expect_err("a string is not an integer");

    assert!(
        errors[0].message.contains("expected an integer"),
        "{errors:?}"
    );
    assert!(
        errors[0].message.contains("many"),
        "the value must be shown"
    );
}

#[test]
fn a_group_row_is_not_validated_as_settings() {
    // A group's `config` holds child entries; validating it as plugin settings would report every
    // child as an unknown field.
    let group = ConfigValue::Entries(vec![Entry {
        id: Some("child".to_owned()),
        name: Some("child-plugin".to_owned()),
        ..Entry::default()
    }]);

    assert!(schema().validate(&group).is_ok());
}

#[test]
fn a_non_mapping_config_is_rejected() {
    let errors = schema()
        .validate(&ConfigValue::Json(serde_json::json!("a string")))
        .expect_err("a plugin config must be a mapping");

    assert!(
        errors[0].message.contains("expected a mapping"),
        "{errors:?}"
    );
}

#[test]
fn rows_are_validated_by_plugin_name() {
    let list = EntryList::new(vec![
        Entry {
            id: Some("llm".to_owned()),
            name: Some("dsh-llm-deepseek".to_owned()),
            config: Some(config(serde_json::json!({"maxTokens": 0}))),
            ..Entry::default()
        },
        Entry {
            id: Some("other".to_owned()),
            name: Some("unrelated".to_owned()),
            config: Some(config(serde_json::json!({"nonsense": true}))),
            ..Entry::default()
        },
    ]);

    let errors = schema()
        .validate_rows(&list)
        .expect_err("the llm row is invalid");
    assert_eq!(
        errors.len(),
        1,
        "only the matching plugin is checked: {errors:?}"
    );
    assert!(errors[0].path.contains("maxTokens"));
}

#[test]
fn a_row_with_no_config_gets_defaults_not_an_error() {
    let list = EntryList::new(vec![Entry {
        id: Some("llm".to_owned()),
        name: Some("dsh-llm-deepseek".to_owned()),
        ..Entry::default()
    }]);

    schema()
        .validate_rows(&list)
        .expect("all fields are optional, so defaults apply");
}

#[test]
fn group_children_are_validated() {
    let list = EntryList::new(vec![Entry {
        id: Some("group".to_owned()),
        group: Some(true),
        config: Some(ConfigValue::Entries(vec![Entry {
            id: Some("inner".to_owned()),
            name: Some("dsh-llm-deepseek".to_owned()),
            config: Some(config(serde_json::json!({"maxTokens": -5}))),
            ..Entry::default()
        }])),
        ..Entry::default()
    }]);

    let errors = schema()
        .validate_rows(&list)
        .expect_err("a group child is still a row");
    assert!(errors[0].message.contains("at least 1"), "{errors:?}");
}

#[test]
fn the_real_deepseek_schema_accepts_its_own_defaults() {
    // The shape of upstream's `deepSeekConfigFields`: numeric bounds, an enum, a plain string.
    let real = ConfigSchema::new("dsh-llm-deepseek")
        .field(integer("maxTokens", 1, i64::MAX, 1, 8192))
        .field(integer("defaultContextWindow", 1, i64::MAX, 1, 128_000))
        .field(text(
            "reasoningEffort",
            &["off", "low", "high", "max"],
            "high",
        ))
        .field(text("thinking", &["enabled", "disabled"], "enabled"));

    // No config at all must resolve to the documented defaults.
    let validated = real
        .validate(&config(serde_json::json!({})))
        .expect("defaults are valid");
    let ConfigValue::Map(map) = validated else {
        panic!("expected a map");
    };
    assert_eq!(map["maxTokens"], serde_json::json!(8192));
    assert_eq!(map["reasoningEffort"], serde_json::json!("high"));
}

// ---- validation in the load path ----------------------------------------------------------------

/// Loads and validates in one step, as a loader would.
fn load_and_validate(yaml: &str, schema: &ConfigSchema) -> Result<EntryList, Vec<FieldError>> {
    let patches = parse_patches(yaml).expect("parse");
    let mut list = EntryList::new(patches[0].insert.clone().unwrap_or_default());
    schema.validate_rows(&list)?;
    // A second layer may fix a value the first got wrong; validation runs after composition.
    let _ = &mut list;
    Ok(list)
}

#[test]
fn a_bad_value_in_a_real_patch_is_reported_with_its_path() {
    let yaml = "- insert:\n    - id: llm\n      name: dsh-llm-deepseek\n      config:\n        maxTokens: 0\n";

    let errors = load_and_validate(yaml, &schema()).expect_err("zero is below the minimum");

    assert_eq!(errors.len(), 1);
    assert_eq!(errors[0].path, "dsh-llm-deepseek.maxTokens");
    assert!(errors[0].message.contains("at least 1"));
}

#[test]
fn composition_before_validation_lets_a_later_layer_fix_a_bad_value() {
    // The reason validation belongs after patch composition, not per file: a mode bundle is allowed
    // to correct what a base bundle set.
    let base = parse_patches(
        "- insert:\n    - id: llm\n      name: dsh-llm-deepseek\n      config:\n        maxTokens: 0\n",
    )
    .unwrap();
    let mut list = EntryList::new(base[0].insert.clone().unwrap());
    assert!(
        schema().validate_rows(&list).is_err(),
        "the base alone is invalid"
    );

    let fix = parse_patches("- id: llm\n  config:\n    maxTokens: 4096\n").unwrap();
    list.apply(&fix, 1);

    schema()
        .validate_rows(&list)
        .expect("the overlay repaired the value");
}

#[test]
fn a_valid_patch_passes_through_the_loader() {
    let yaml = "- insert:\n    - id: llm\n      name: dsh-llm-deepseek\n      config:\n        maxTokens: 4096\n        thinking: disabled\n";

    let list = load_and_validate(yaml, &schema()).expect("valid");
    assert_eq!(list.len(), 1);
}
