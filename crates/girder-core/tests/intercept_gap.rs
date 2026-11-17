//! Tests for service intercept config.
//!
//! Upstream's `ctx.intercept(name, config)` is four lines of `Object.create`, and the behaviour it
//! buys is stated in exactly two comments: *"Entries added closer to the root apply first"* and
//! *"`base` is prepended and `head` appended"*. Each test below pins one of the rules those
//! comments name, plus the invisibility properties that `Object.create` gives for free upstream and
//! that a parent-pointer chain has to be written carefully to keep.
//!
//! # Why the module is compiled by path
//!
//! `intercept_gap` is not exported from the crate root yet; the wiring is a separate change to
//! `lib.rs`, which this module deliberately does not touch. Compiling the exact source file here
//! keeps these tests runnable and independent of that wiring, while still exercising the same code
//! a consumer will reach through `girder_core::intercept_gap`. Nothing is duplicated: this is the
//! real file.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    reason = "tests assert by panicking; a failed unwrap is a failed test"
)]

#[path = "../src/intercept_gap.rs"]
mod intercept_gap;

use serde_json::json;

use intercept_gap::{Config, InterceptError, Intercepts};

// ---- fixtures -----------------------------------------------------------------------------------

/// A config from `key = value` pairs, in the order a JavaScript object literal would list them.
fn config(pairs: &[(&str, serde_json::Value)]) -> Config {
    Config::from_pairs(pairs.iter().map(|(key, value)| (*key, value.clone())))
}

/// The level a resolved config names, which every ordering test reads.
///
/// Using one shared key across layers is what makes an ordering bug visible: a merge that walks
/// the chain the wrong way still produces *a* config, just with the wrong winner.
fn level_of(config: &Config) -> Option<i64> {
    config.get("level").and_then(serde_json::Value::as_i64)
}

/// A three-deep chain, each level intercepting `logger` with a distinct `level`.
///
/// Mirrors the shape upstream's doc comment describes: the root-most entry applies first, so the
/// nearest entry — `level = 3` — is the one a reader below the third context sees.
fn three_levels() -> (Intercepts, Intercepts, Intercepts, Intercepts) {
    let root = Intercepts::root();
    let first = root.intercept("logger", config(&[("level", json!(1))]));
    let second = first.intercept("logger", config(&[("level", json!(2))]));
    let third = second.intercept("logger", config(&[("level", json!(3))]));
    (root, first, second, third)
}

// ---- rule 1: ancestor entries apply first -------------------------------------------------------

#[test]
fn the_nearest_entry_wins_a_shared_key() {
    // The single property that would break first if the chain were walked leaf-to-root.
    let (_, _, _, third) = three_levels();

    assert_eq!(
        level_of(&third.resolve_config("logger", None, None)),
        Some(3)
    );
}

#[test]
fn each_context_in_the_chain_sees_its_own_depth() {
    // A context's config is what *it* would resolve, so a reader at each depth must see that
    // depth's cumulative state -- not the deepest one, which is what sharing a mutable map would
    // produce.
    let (root, first, second, third) = three_levels();

    assert_eq!(level_of(&root.resolve_config("logger", None, None)), None);
    assert_eq!(
        level_of(&first.resolve_config("logger", None, None)),
        Some(1)
    );
    assert_eq!(
        level_of(&second.resolve_config("logger", None, None)),
        Some(2)
    );
    assert_eq!(
        level_of(&third.resolve_config("logger", None, None)),
        Some(3)
    );
}

#[test]
fn layers_are_ordered_root_most_first() {
    // `resolveConfig` builds the list with `unshift` in a root-ward walk, so the final order runs
    // root-most to nearest. A custom merge sees this order, and it is the order a service that
    // concatenates arrays depends on.
    let (_, _, _, third) = three_levels();
    let layers = third.layers("logger", None, None);

    let levels: Vec<Option<i64>> = layers.iter().map(|config| level_of(config)).collect();
    assert_eq!(levels, vec![Some(1), Some(2), Some(3)]);
}

#[test]
fn a_key_no_later_layer_names_survives() {
    // Shallow merge replaces only the keys a later layer actually sets. A layer that named only
    // `level` must not drop the earlier layer's `name`.
    let root = Intercepts::root();
    let first = root.intercept(
        "logger",
        config(&[("name", json!("app")), ("level", json!(1))]),
    );
    let second = first.intercept("logger", config(&[("level", json!(2))]));

    let resolved = second.resolve_config("logger", None, None);
    assert_eq!(level_of(&resolved), Some(2));
    assert_eq!(resolved.get("name"), Some(&json!("app")));
}

#[test]
fn an_empty_layer_contributes_nothing_and_costs_nothing() {
    // `{}` is truthy in JavaScript, so an empty intercept is a real entry that merges nothing.
    let root = Intercepts::root();
    let child = root.intercept("logger", config(&[]));

    assert_eq!(child.resolve_config("logger", None, None), Config::new());
    assert_eq!(child.layers("logger", None, None).len(), 1);
}

// ---- rule 2: base is prepended, head is appended ------------------------------------------------

#[test]
fn base_loses_and_head_wins() {
    // Both bracket every intercept, so a caller-supplied `head` overrides an intercept and an
    // intercept overrides a service-supplied `base`.
    let (_, first, _, _) = three_levels();
    let base = config(&[("level", json!(0)), ("from_base", json!(true))]);
    let head = config(&[("level", json!(9)), ("from_head", json!(true))]);

    let resolved = first.resolve_config("logger", Some(&base), Some(&head));

    assert_eq!(level_of(&resolved), Some(9), "head has the last word");
    assert_eq!(resolved.get("from_base"), Some(&json!(true)));
    assert_eq!(resolved.get("from_head"), Some(&json!(true)));
}

#[test]
fn base_and_head_bracket_the_intercept_band_in_the_layer_list() {
    // Not just the merged value: the position matters for a custom merge.
    let (_, first, _, _) = three_levels();
    let base = config(&[("level", json!(0))]);
    let head = config(&[("level", json!(9))]);

    let layers = first.layers("logger", Some(&base), Some(&head));
    let levels: Vec<Option<i64>> = layers.iter().map(|config| level_of(config)).collect();

    assert_eq!(levels, vec![Some(0), Some(1), Some(9)]);
}

#[test]
fn base_alone_still_resolves_when_no_intercept_applies() {
    // A service that is never intercepted must still see what its caller passed.
    let root = Intercepts::root();
    let base = config(&[("level", json!(5))]);

    assert_eq!(
        level_of(&root.resolve_config("logger", Some(&base), None)),
        Some(5)
    );
}

#[test]
fn a_custom_merge_receives_the_same_inputs_as_the_default() {
    // Upstream spreads the list into `Config.merge(...configs)`; the closure here is that call.
    // The test concatenates, which shallow merge cannot do -- `Object.assign` would replace the
    // array -- and asserts the order the concatenation depends on.
    let root = Intercepts::root();
    let first = root.intercept("tools", config(&[("enabled", json!(["a"]))]));
    let second = first.intercept("tools", config(&[("enabled", json!(["b"]))]));
    let head = config(&[("enabled", json!(["z"]))]);

    let resolved: Vec<String> = second.resolve_config_with("tools", None, Some(&head), |layers| {
        let default = Config::merge_shallow(layers);
        default
            .get("enabled")
            .and_then(serde_json::Value::as_array)
            .map(|items| {
                items
                    .iter()
                    .filter_map(serde_json::Value::as_str)
                    .map(str::to_owned)
                    .collect()
            })
            .unwrap_or_default()
    });

    // The default merge is last-wins, so `head` is what a shallow merge would have left.
    assert_eq!(resolved, vec!["z"]);

    // The custom merge sees all three in order and can concatenate instead.
    let concatenated: Vec<String> =
        second.resolve_config_with("tools", None, Some(&head), |layers| {
            layers
                .iter()
                .filter_map(|config| config.get("enabled"))
                .filter_map(serde_json::Value::as_array)
                .flatten()
                .filter_map(serde_json::Value::as_str)
                .map(str::to_owned)
                .collect()
        });
    assert_eq!(concatenated, vec!["a", "b", "z"]);
}

// ---- rule 3: merge is shallow, per key ----------------------------------------------------------

#[test]
fn a_later_layer_replaces_a_nested_object_wholesale() {
    // `Object.assign` is shallow, so `{"a": {"x": 1, "y": 2}}` overridden by `{"a": {"x": 9}}`
    // loses `y`. This is the case a service declares `Config.merge` to escape, and a rewrite that
    // "helpfully" merged deeply would silently change every service's behaviour.
    let root = Intercepts::root();
    let first = root.intercept("db", config(&[("pool", json!({ "min": 1, "max": 8 }))]));
    let second = first.intercept("db", config(&[("pool", json!({ "max": 64 }))]));

    let resolved = second.resolve_config("db", None, None);

    assert_eq!(resolved.get("pool"), Some(&json!({ "max": 64 })));
}

#[test]
fn merge_shallow_of_no_layers_is_empty() {
    // The identity case, which a service with no intercept and no base/head resolves.
    assert_eq!(Config::merge_shallow(&[]), Config::new());
    assert!(Config::merge_shallow(&[]).is_empty());
}

// ---- the two invisibility properties ------------------------------------------------------------

#[test]
fn the_parent_is_never_affected_by_a_child() {
    // Upstream gets this from `Object.create`; a mutable shared map would fail it.
    let root = Intercepts::root();
    let child = root.intercept("logger", config(&[("level", json!(1))]));

    assert_eq!(
        level_of(&child.resolve_config("logger", None, None)),
        Some(1)
    );
    assert_eq!(
        level_of(&root.resolve_config("logger", None, None)),
        None,
        "deriving a child must leave the parent untouched"
    );
    assert!(root.layers("logger", None, None).is_empty());
}

#[test]
fn a_sibling_never_sees_its_brothers_entry() {
    // Two children of one parent are independent subtrees, not a shared bucket.
    let root = Intercepts::root();
    let left = root.intercept("logger", config(&[("level", json!(1))]));
    let right = root.intercept("logger", config(&[("level", json!(2))]));

    assert_eq!(
        level_of(&left.resolve_config("logger", None, None)),
        Some(1)
    );
    assert_eq!(
        level_of(&right.resolve_config("logger", None, None)),
        Some(2)
    );
    assert_ne!(left, right, "distinct derivations are distinct scopes");
}

#[test]
fn entries_do_not_leak_across_service_names() {
    // The map is keyed by name, so intercepting `logger` must not configure `tools` -- and a
    // lookup at a name nobody intercepts must not walk into an unrelated entry.
    let root = Intercepts::root();
    let child = root.intercept("logger", config(&[("level", json!(1))]));

    assert!(child.layers("tools", None, None).is_empty());
    assert_eq!(child.resolve_config("tools", None, None), Config::new());
}

#[test]
fn the_same_name_intercepted_under_different_names_is_independent() {
    // Two entries on one context, for two services: each resolves only its own.
    let root = Intercepts::root();
    let child = root
        .intercept("logger", config(&[("level", json!(1))]))
        .intercept("tools", config(&[("level", json!(2))]));

    assert_eq!(
        level_of(&child.resolve_config("logger", None, None)),
        Some(1)
    );
    assert_eq!(
        level_of(&child.resolve_config("tools", None, None)),
        Some(2)
    );
}

#[test]
fn a_clone_shares_the_scope() {
    // `Registry::isolate(name, Some(label))` joins two subtrees by label; the analogue here is
    // that a clone is the same node, so a layer added to one is visible in the other.
    let root = Intercepts::root();
    let branch = root.intercept("logger", config(&[("level", json!(1))]));
    let sibling = branch.clone();
    let deeper = sibling.intercept("logger", config(&[("level", json!(2))]));

    assert_eq!(branch, sibling);
    assert_eq!(
        level_of(&branch.resolve_config("logger", None, None)),
        Some(1),
        "the clone is the same scope"
    );
    assert_eq!(
        level_of(&deeper.resolve_config("logger", None, None)),
        Some(2),
        "a layer below the shared node is still visible to it"
    );
}

// ---- isolation: the config-side shadow ----------------------------------------------------------

#[test]
fn an_isolated_child_does_not_inherit_the_parents_config() {
    // The property `Registry::isolate` already provides for services, stated for config: a lookup
    // through the isolated child never sees the parent's entry.
    let root = Intercepts::root();
    let parent = root.intercept(
        "logger",
        config(&[("level", json!(1)), ("name", json!("app"))]),
    );
    let child = parent.isolate("logger");

    assert_eq!(
        child.resolve_config("logger", None, None),
        Config::new(),
        "an isolated child starts from nothing"
    );
}

#[test]
fn isolation_hides_only_the_named_service() {
    // Isolation is per name, exactly like `Registry::isolate`; a sibling service keeps inheriting.
    let root = Intercepts::root();
    let parent = root
        .intercept("logger", config(&[("level", json!(1))]))
        .intercept("tools", config(&[("level", json!(2))]));
    let child = parent.isolate("logger");

    assert_eq!(child.resolve_config("logger", None, None), Config::new());
    assert_eq!(
        level_of(&child.resolve_config("tools", None, None)),
        Some(2)
    );
}

#[test]
fn a_layer_added_below_an_isolation_node_still_applies() {
    // The shadow cuts the ancestors, not the descendants.
    let root = Intercepts::root();
    let parent = root.intercept("logger", config(&[("level", json!(1))]));
    let isolated = parent.isolate("logger");
    let child = isolated.intercept("logger", config(&[("level", json!(7))]));

    assert_eq!(
        level_of(&child.resolve_config("logger", None, None)),
        Some(7)
    );
}

#[test]
fn the_parent_still_resolves_its_own_config_after_a_child_isolates() {
    // Mirror of the service-side test: isolating in a child must not disturb the parent.
    let root = Intercepts::root();
    let parent = root.intercept("logger", config(&[("level", json!(1))]));
    let _child = parent.isolate("logger");

    assert_eq!(
        level_of(&parent.resolve_config("logger", None, None)),
        Some(1)
    );
}

#[test]
fn a_sibling_still_inherits_what_a_brother_isolated_away() {
    let root = Intercepts::root();
    let parent = root.intercept("logger", config(&[("level", json!(1))]));
    let isolated = parent.isolate("logger");
    let plain = parent.intercept("logger", config(&[("level", json!(2))]));

    assert_eq!(isolated.resolve_config("logger", None, None), Config::new());
    assert_eq!(
        level_of(&plain.resolve_config("logger", None, None)),
        Some(2)
    );
}

#[test]
fn isolating_an_unintercepted_name_is_harmless() {
    let root = Intercepts::root();
    let child = root
        .isolate("logger")
        .intercept("logger", config(&[("level", json!(1))]));

    assert_eq!(
        level_of(&child.resolve_config("logger", None, None)),
        Some(1)
    );
}

// ---- config acquisition -------------------------------------------------------------------------

#[test]
fn a_config_is_an_object_of_keys() {
    let config = Config::try_from_object(json!({ "level": "info", "name": "app" })).unwrap();

    assert_eq!(config.len(), 2);
    assert!(!config.is_empty());
    assert_eq!(config.get("level"), Some(&json!("info")));
    assert_eq!(config.keys().collect::<Vec<_>>(), vec!["level", "name"]);
}

#[test]
fn a_non_object_config_is_rejected_by_name() {
    // `Object.assign` would spread `"ab"` into `{0: "a", 1: "b"}`, which hides a caller's mistake.
    // The error names the type so the fix is obvious.
    for (value, expected) in [
        (json!(null), "null"),
        (json!([1, 2]), "array"),
        (json!("ab"), "string"),
        (json!(7), "number"),
        (json!(true), "boolean"),
    ] {
        let error = Config::try_from_object(value).unwrap_err();
        assert_eq!(error, InterceptError::NotAnObject { found: expected });
    }
}

#[test]
fn a_repeated_key_in_from_pairs_keeps_the_last_value() {
    let config = config(&[("level", json!(1)), ("level", json!(2))]);

    assert_eq!(config.len(), 1);
    assert_eq!(level_of(&config), Some(2));
}

#[test]
fn insert_reports_the_value_it_displaced() {
    let mut config = config(&[("level", json!(1))]);

    assert_eq!(config.insert("level", json!(2)), Some(json!(1)));
    assert_eq!(config.insert("fresh", json!(3)), None);
    assert_eq!(level_of(&config), Some(2));
}

#[test]
fn a_resolved_config_can_be_handed_back_as_plain_keys() {
    // The consumer of a resolved config is a service constructor that wants the mapping, not the
    // wrapper -- `Object.assign({}, ...)` returns exactly that. Keys stay sorted so the handoff is
    // deterministic.
    let root = Intercepts::root();
    let child = root.intercept(
        "logger",
        config(&[("name", json!("app")), ("level", json!(2))]),
    );

    let map = child.resolve_config("logger", None, None).into_map();

    assert_eq!(map.len(), 2);
    assert_eq!(map.get("level"), Some(&json!(2)));
    assert_eq!(
        map.keys().map(String::as_str).collect::<Vec<_>>(),
        vec!["level", "name"]
    );
}

#[test]
fn the_default_handle_is_a_root_with_no_layers() {
    // `Default` exists so a caller that has no intercept scope yet does not have to special-case
    // resolution; it must behave exactly like `root()`.
    let handle = Intercepts::default();

    assert!(handle.layers("logger", None, None).is_empty());
    assert_eq!(handle.resolve_config("logger", None, None), Config::new());
    assert_eq!(
        handle.resolve_config("logger", None, None),
        Intercepts::root().resolve_config("logger", None, None)
    );
}

#[test]
fn two_roots_are_the_same_configuration_but_not_the_same_scope() {
    // `PartialEq` compares node identity, which is what joins two subtrees. Two independent roots
    // therefore compare unequal even though they resolve identically -- the analogue of two
    // `Registry::isolate(name, None)` calls, which get fresh labels rather than a shared one.
    let left = Intercepts::root();
    let right = Intercepts::root();

    assert_ne!(left, right);
    assert_eq!(
        left.resolve_config("logger", None, None),
        right.resolve_config("logger", None, None)
    );
}
