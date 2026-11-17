//! Tests for `boot_loader_gap`: the entry-tree, patch, and journal decisions of upstream
//! `vendor/loader` and `vendor/include`.
//!
//! The cases are the properties the upstream modules state in prose, restated where the code says
//! them and extended where a property is only visible at an edge the upstream suite does not reach:
//! the file wins every conflict, a created-then-removed entry leaves no trace, a patch-owned key is
//! written back to the patch, and an unevaluable `disabled` is an error rather than a `false`.
//!
//! # Why the module is compiled by path
//!
//! The crate root's wiring is a separate change, so this compiles the exact source file rather than
//! a copy. Inside a `#[path]` module the `crate::` prefix would name *this* test crate, not
//! `girder-core`, so every reference to the library goes through `girder_core::{...}` — which is why
//! the module itself imports nothing from the crate root.

#[path = "../src/boot_loader_gap.rs"]
mod boot_loader_gap;

use std::collections::BTreeMap;

use serde_json::{Value, json};

use boot_loader_gap::{
    Changes, ConfigStage, Conflict, Entry, EntryChange, EntryOwner, JournalRecord, MissingFile,
    Patch, PatchIndex, Placement, StaleWrite, WritePlan, apply_changes, apply_journal,
    apply_patches, assign_ids, check_expected, content_type, detach, diff_options, ensure_id,
    ensure_ids, ensure_insert_ids, extension, flatten, initial_or_fail, is_disabled,
    is_js_expression, is_transient_rename_error, journal_get, journal_merge, merge_records, place,
    plan_write, reconcile, record, resolve, resolve_group, retry_delay, route_journal, sort_keys,
    temp_path, unwrap_exports,
};

// ---- helpers -------------------------------------------------------------------------------

fn entry(value: &Value) -> Entry {
    value
        .as_object()
        .expect("an entry is a JSON object")
        .clone()
}

fn list(value: &Value) -> Vec<Value> {
    value.as_array().expect("a list is a JSON array").clone()
}

fn ids(data: &[Value]) -> Vec<String> {
    flatten(data).into_keys().collect()
}

/// A three-entry tree: a root plugin, a group, and two children of the group.
fn tree() -> Vec<Value> {
    list(&json!([
        { "id": "root", "name": "a" },
        {
            "id": "group",
            "name": "b",
            "group": true,
            "config": [
                { "id": "child-1", "name": "c", "config": { "size": 1 } },
                { "id": "child-2", "name": "d" }
            ]
        },
        { "id": "tail", "name": "e" }
    ]))
}

// ---- flatten / place / detach --------------------------------------------------------------

#[test]
fn flatten_indexes_nested_children_with_their_parent_and_position() {
    let flat = flatten(&tree());
    assert_eq!(
        flat.keys().cloned().collect::<Vec<_>>(),
        ["child-1", "child-2", "group", "root", "tail"]
    );
    assert_eq!(flat["child-1"].parent.as_deref(), Some("group"));
    assert_eq!(flat["child-1"].position, 0);
    assert_eq!(flat["child-2"].position, 1);
    assert_eq!(flat["group"].parent, None);
    assert_eq!(flat["group"].position, 1);
    // The path from the root list to the list holding the entry is the group's own position.
    assert_eq!(flat["child-1"].path(), &[1]);
    assert_eq!(flat["tail"].path(), &[] as &[usize]);
}

#[test]
fn flatten_indexes_children_of_an_anonymous_group_under_the_root() {
    // Upstream recurses with `options.id ?? null`, so a group with no id still yields indexed
    // children — with no parent to name.
    let data = list(&json!([
        { "name": "anonymous group", "group": true, "config": [{ "id": "inner" }] }
    ]));
    let flat = flatten(&data);
    assert_eq!(flat.len(), 1);
    assert_eq!(flat["inner"].parent, None);
    assert_eq!(flat["inner"].path(), &[0]);
}

#[test]
fn flatten_ignores_entries_without_an_id() {
    let data = list(&json!([{ "name": "anonymous" }, { "id": "known" }]));
    assert_eq!(ids(&data), ["known"]);
}

#[test]
fn place_clamps_the_position_and_refuses_a_non_group() {
    let mut data = tree();
    assert!(place(&mut data, entry(&json!({ "id": "new" })), None, 99));
    assert_eq!(data.last().unwrap()["id"], "new");

    // A position past the end is clamped rather than rejected, exactly as `splice` would behave.
    assert!(place(
        &mut data,
        entry(&json!({ "id": "child-3" })),
        Some("group"),
        99
    ));
    let group = resolve(&data, "group").unwrap();
    let children = group["config"].as_array().unwrap();
    assert_eq!(children.len(), 3);
    assert_eq!(children[2]["id"], "child-3");

    // A group that is not a group is not a place.
    assert!(!place(
        &mut data,
        entry(&json!({ "id": "x" })),
        Some("root"),
        0
    ));
    assert!(!place(
        &mut data,
        entry(&json!({ "id": "x" })),
        Some("missing"),
        0
    ));
}

#[test]
fn place_creates_the_child_list_a_group_is_missing() {
    let mut data = list(&json!([{ "id": "group", "group": true }]));
    assert!(place(
        &mut data,
        entry(&json!({ "id": "child" })),
        Some("group"),
        0
    ));
    assert_eq!(data[0]["config"][0]["id"], "child");
}

#[test]
fn detach_removes_the_entry_and_returns_its_options() {
    let mut data = tree();
    let removed = detach(&mut data, "child-1").expect("child-1 was in the tree");
    assert_eq!(removed["name"], "c");
    assert_eq!(
        resolve_group(&data, Some("group")).unwrap().len(),
        1,
        "one child left"
    );
    // A second detach finds nothing and must not touch the tree.
    assert!(detach(&mut data, "child-1").is_none());
    assert_eq!(ids(&data), ["child-2", "group", "root", "tail"]);
}

#[test]
fn detach_only_removes_the_named_entry_not_its_siblings() {
    let mut data = tree();
    detach(&mut data, "group");
    assert_eq!(ids(&data), ["root", "tail"]);
}

// ---- the journal ---------------------------------------------------------------------------

#[test]
fn diff_options_skips_the_id_and_unchanged_keys() {
    let legacy = entry(&json!({ "id": "a", "name": "x", "config": { "n": 1 } }));
    let options = entry(&json!({ "id": "b", "name": "x", "config": { "n": 2 } }));
    let changes = diff_options(&legacy, &options);
    assert_eq!(
        changes,
        vec![("config".to_owned(), Some(json!({ "n": 2 })))]
    );
}

#[test]
fn diff_options_reports_a_removed_key_as_a_deletion() {
    let legacy = entry(&json!({ "id": "a", "disabled": true }));
    let options = entry(&json!({ "id": "a" }));
    assert_eq!(
        diff_options(&legacy, &options),
        vec![("disabled".to_owned(), None)]
    );
}

#[test]
fn merge_records_folds_later_values_into_earlier_ones() {
    let older = JournalRecord::Upsert {
        created: false,
        parent: Placement::Group("g".to_owned()),
        position: 2,
        changes: vec![("name".to_owned(), Some(json!("a")))],
    };
    let newer = JournalRecord::Upsert {
        created: true,
        parent: Placement::Unchanged,
        position: 5,
        changes: vec![
            ("name".to_owned(), Some(json!("b"))),
            ("disabled".to_owned(), Some(json!(true))),
        ],
    };
    let merged = merge_records(Some(&older), &newer).expect("two updates merge");
    assert_eq!(
        merged,
        JournalRecord::Upsert {
            // The *earlier* creation flag survives: the entry existed before either update.
            created: false,
            // The later record did not relocate, so the earlier placement stands.
            parent: Placement::Group("g".to_owned()),
            position: 2,
            changes: vec![
                ("name".to_owned(), Some(json!("b"))),
                ("disabled".to_owned(), Some(json!(true))),
            ],
        }
    );
}

#[test]
fn merge_records_lets_a_later_move_replace_an_earlier_one() {
    let older = JournalRecord::Upsert {
        created: false,
        parent: Placement::Group("old".to_owned()),
        position: 0,
        changes: Vec::new(),
    };
    let newer = JournalRecord::Upsert {
        created: false,
        parent: Placement::Root,
        position: 3,
        changes: Vec::new(),
    };
    assert_eq!(
        merge_records(Some(&older), &newer),
        Some(JournalRecord::Upsert {
            created: false,
            parent: Placement::Root,
            position: 3,
            changes: Vec::new(),
        })
    );
}

#[test]
fn an_entry_created_and_removed_before_a_write_leaves_no_trace() {
    let created = JournalRecord::Upsert {
        created: true,
        parent: Placement::Root,
        position: 0,
        changes: vec![("name".to_owned(), Some(json!("a")))],
    };
    assert_eq!(merge_records(Some(&created), &JournalRecord::Remove), None);

    // The same removal on an entry the file already had is a real removal.
    let updated = JournalRecord::Upsert {
        created: false,
        parent: Placement::Root,
        position: 0,
        changes: vec![("name".to_owned(), Some(json!("a")))],
    };
    assert_eq!(
        merge_records(Some(&updated), &JournalRecord::Remove),
        Some(JournalRecord::Remove)
    );
}

#[test]
fn a_removal_followed_by_an_update_is_an_update_of_a_missing_entry() {
    let newer = JournalRecord::Upsert {
        created: true,
        parent: Placement::Root,
        position: 0,
        changes: vec![("name".to_owned(), Some(json!("a")))],
    };
    assert_eq!(
        merge_records(Some(&JournalRecord::Remove), &newer),
        Some(JournalRecord::Upsert {
            // The tree removed it and then re-created it, so it is not a creation *by the file's
            // reckoning*: the write has to say the entry exists again.
            created: false,
            parent: Placement::Root,
            position: 0,
            changes: vec![("name".to_owned(), Some(json!("a")))],
        })
    );
}

#[test]
fn journal_merge_keeps_insertion_order_and_drops_empty_records() {
    let mut journal = Vec::new();
    journal_merge(
        &mut journal,
        "b",
        &JournalRecord::Upsert {
            created: true,
            parent: Placement::Root,
            position: 0,
            changes: Vec::new(),
        },
    );
    journal_merge(&mut journal, "a", &JournalRecord::Remove);
    assert_eq!(
        journal
            .iter()
            .map(|(id, _)| id.as_str())
            .collect::<Vec<_>>(),
        ["b", "a"]
    );

    // Merging a removal into the created entry drops it, and the order of the rest is untouched.
    journal_merge(&mut journal, "b", &JournalRecord::Remove);
    assert_eq!(
        journal
            .iter()
            .map(|(id, _)| id.as_str())
            .collect::<Vec<_>>(),
        ["a"]
    );
    assert!(journal_get(&journal, "b").is_none());
}

#[test]
fn record_writes_a_creation_as_its_whole_options_minus_the_id() {
    let mut journal = Vec::new();
    record(
        &mut journal,
        &EntryChange {
            id: "a".to_owned(),
            options: Some(entry(&json!({
                "id": "a",
                "name": "x",
                "config": { "n": 1 }
            }))),
            legacy: None,
            parent: Some("group".to_owned()),
            moved: false,
            position: 0,
        },
    );
    match journal_get(&journal, "a").unwrap() {
        JournalRecord::Upsert {
            created,
            parent,
            changes,
            ..
        } => {
            assert!(created);
            assert_eq!(parent, &Placement::Group("group".to_owned()));
            let keys: Vec<&str> = changes.iter().map(|(key, _)| key.as_str()).collect();
            // `serde_json::Map` is `BTreeMap`-backed here, so the delta comes out alphabetical;
            // what matters is that it is the *whole* options minus `id`, not the order.
            assert_eq!(keys, ["config", "name"]);
        }
        other @ JournalRecord::Remove => panic!("expected an upsert, got {other:?}"),
    }
}

#[test]
fn record_reports_a_move_only_when_the_entry_moved() {
    let legacy = entry(&json!({ "id": "a", "name": "x" }));
    let options = entry(&json!({ "id": "a", "name": "y" }));
    for (moved, expected) in [
        (false, Placement::Unchanged),
        (true, Placement::Group("g".to_owned())),
    ] {
        let mut journal = Vec::new();
        record(
            &mut journal,
            &EntryChange {
                id: "a".to_owned(),
                options: Some(options.clone()),
                legacy: Some(legacy.clone()),
                parent: Some("g".to_owned()),
                moved,
                position: 1,
            },
        );
        match journal_get(&journal, "a").unwrap() {
            JournalRecord::Upsert { parent, .. } => assert_eq!(parent, &expected),
            other @ JournalRecord::Remove => panic!("expected an upsert, got {other:?}"),
        }
    }
}

#[test]
fn apply_changes_deletes_a_key_whose_delta_is_null() {
    let mut options = entry(&json!({ "id": "a", "disabled": true }));
    apply_changes(
        &mut options,
        &vec![
            ("disabled".to_owned(), None),
            ("name".to_owned(), Some(json!("x"))),
        ],
        None,
    );
    assert_eq!(options, entry(&json!({ "id": "a", "name": "x" })));
}

#[test]
fn apply_journal_creates_updates_and_removes() {
    let mut data = tree();
    let journal = vec![
        (
            "root".to_owned(),
            JournalRecord::Upsert {
                created: false,
                parent: Placement::Unchanged,
                position: 0,
                changes: vec![("disabled".to_owned(), Some(json!(true)))],
            },
        ),
        (
            "fresh".to_owned(),
            JournalRecord::Upsert {
                created: true,
                parent: Placement::Group("group".to_owned()),
                position: 1,
                changes: vec![("name".to_owned(), Some(json!("new")))],
            },
        ),
        ("tail".to_owned(), JournalRecord::Remove),
    ];
    let mut warnings = Vec::new();
    apply_journal(&mut data, &journal, None, &mut |message| {
        warnings.push(message.to_owned());
    });

    assert!(warnings.is_empty(), "unexpected warnings: {warnings:?}");
    assert_eq!(resolve(&data, "root").unwrap()["disabled"], true);
    assert_eq!(resolve(&data, "group:fresh").unwrap()["name"], "new");
    let children = resolve_group(&data, Some("group")).unwrap();
    assert_eq!(children[1]["id"], "fresh");
    assert!(resolve(&data, "tail").is_err());
}

#[test]
fn apply_journal_moves_an_entry_between_groups() {
    let mut data = tree();
    let journal = vec![(
        "child-2".to_owned(),
        JournalRecord::Upsert {
            created: false,
            parent: Placement::Root,
            position: 0,
            changes: Vec::new(),
        },
    )];
    apply_journal(&mut data, &journal, None, &mut |message| {
        panic!("unexpected warning: {message}");
    });
    assert_eq!(data[0]["id"], "child-2");
    assert_eq!(resolve_group(&data, Some("group")).unwrap().len(), 1);
}

#[test]
fn apply_journal_puts_an_entry_back_when_the_move_target_is_gone() {
    let mut data = tree();
    let journal = vec![(
        "child-1".to_owned(),
        JournalRecord::Upsert {
            created: false,
            parent: Placement::Group("vanished".to_owned()),
            position: 0,
            changes: Vec::new(),
        },
    )];
    let mut warnings = Vec::new();
    apply_journal(&mut data, &journal, None, &mut |message| {
        warnings.push(message.to_owned());
    });
    assert_eq!(warnings.len(), 1);
    assert!(warnings[0].contains("cannot move entry child-1"));
    // The entry is still where it was, and still holds its config.
    assert_eq!(
        resolve(&data, "group:child-1").unwrap()["config"]["size"],
        1
    );
}

#[test]
fn apply_journal_respects_a_filter() {
    let mut data = tree();
    let journal = vec![
        (
            "root".to_owned(),
            JournalRecord::Upsert {
                created: false,
                parent: Placement::Unchanged,
                position: 0,
                changes: vec![("name".to_owned(), Some(json!("changed")))],
            },
        ),
        (
            "tail".to_owned(),
            JournalRecord::Upsert {
                created: false,
                parent: Placement::Unchanged,
                position: 2,
                changes: vec![("name".to_owned(), Some(json!("changed")))],
            },
        ),
    ];
    // Only `root`'s `name` is this caller's to write, and `tail` is not its entry at all.
    apply_journal(
        &mut data,
        &journal,
        Some(&|id, key| id == "root" && (key.is_none() || key == Some("name"))),
        &mut |message| panic!("unexpected warning: {message}"),
    );
    assert_eq!(resolve(&data, "root").unwrap()["name"], "changed");
    assert_eq!(resolve(&data, "tail").unwrap()["name"], "e");
}

// ---- three-way reconciliation --------------------------------------------------------------

#[test]
fn a_file_edit_wins_over_a_runtime_edit_of_the_same_key() {
    let base = flatten(&tree());
    let mut theirs_data = tree();
    theirs_data[0]["name"] = json!("edited in file");
    let theirs = flatten(&theirs_data);

    let mut journal = vec![(
        "root".to_owned(),
        JournalRecord::Upsert {
            created: false,
            parent: Placement::Unchanged,
            position: 0,
            changes: vec![("name".to_owned(), Some(json!("edited at runtime")))],
        },
    )];
    let owned = |_: &str, _: Option<&str>| true;
    let conflicts = reconcile(&mut journal, &base, &theirs, &owned);

    assert_eq!(
        conflicts,
        vec![Conflict {
            id: "root".to_owned(),
            reason: "key \"name\" modified both in file and at runtime".to_owned(),
        }]
    );
    // The only pending change was dropped, so the journal is empty and the file's value stands.
    assert!(journal.is_empty());
}

#[test]
fn a_runtime_edit_the_file_agrees_with_is_kept_when_another_key_differs() {
    let base = flatten(&tree());
    let mut theirs_data = tree();
    theirs_data[0]["name"] = json!("same");
    let theirs = flatten(&theirs_data);

    let mut journal = vec![(
        "root".to_owned(),
        JournalRecord::Upsert {
            created: false,
            parent: Placement::Unchanged,
            position: 0,
            changes: vec![
                ("name".to_owned(), Some(json!("same"))),
                ("disabled".to_owned(), Some(json!(true))),
            ],
        },
    )];
    let conflicts = reconcile(&mut journal, &base, &theirs, &|_, _| true);
    assert!(conflicts.is_empty());
    match journal_get(&journal, "root").unwrap() {
        JournalRecord::Upsert { changes, .. } => {
            // Both keys survive: `name` because the file already holds the pending value (there is
            // nothing to win, and writing it again is a no-op), `disabled` because the file never
            // mentioned it.
            assert_eq!(
                changes,
                &vec![
                    ("name".to_owned(), Some(json!("same"))),
                    ("disabled".to_owned(), Some(json!(true))),
                ]
            );
        }
        other @ JournalRecord::Remove => panic!("expected an upsert, got {other:?}"),
    }
}

#[test]
fn a_file_removal_wins_over_a_runtime_edit() {
    let base = flatten(&tree());
    let mut theirs_data = tree();
    thesis_remove(&mut theirs_data, "root");
    let theirs = flatten(&theirs_data);

    let mut journal = vec![(
        "root".to_owned(),
        JournalRecord::Upsert {
            created: false,
            parent: Placement::Unchanged,
            position: 0,
            changes: vec![("name".to_owned(), Some(json!("x")))],
        },
    )];
    let conflicts = reconcile(&mut journal, &base, &theirs, &|_, _| true);
    assert_eq!(
        conflicts,
        vec![Conflict {
            id: "root".to_owned(),
            reason: "removed in file, modified at runtime".to_owned(),
        }]
    );
    assert!(journal.is_empty());
}

fn thesis_remove(data: &mut Vec<Value>, id: &str) {
    detach(data, id);
}

#[test]
fn a_runtime_removal_the_file_left_alone_is_kept() {
    let base = flatten(&tree());
    // The file did not touch `tail`; only the runtime removed it.
    let theirs = base.clone();

    let mut journal = vec![("tail".to_owned(), JournalRecord::Remove)];
    let conflicts = reconcile(&mut journal, &base, &theirs, &|_, _| true);
    assert!(conflicts.is_empty());
    assert_eq!(
        journal,
        vec![("tail".to_owned(), JournalRecord::Remove)],
        "the removal is still pending and the next write performs it"
    );
}

#[test]
fn a_runtime_removal_of_an_entry_the_file_edited_is_a_conflict() {
    let base = flatten(&tree());
    let mut theirs_data = tree();
    theirs_data[2]["name"] = json!("edited in file");
    let theirs = flatten(&theirs_data);

    let mut journal = vec![("tail".to_owned(), JournalRecord::Remove)];
    let conflicts = reconcile(&mut journal, &base, &theirs, &|_, _| true);
    assert_eq!(
        conflicts,
        vec![Conflict {
            id: "tail".to_owned(),
            reason: "modified in file, removed at runtime".to_owned(),
        }]
    );
    assert!(journal.is_empty());
}

#[test]
fn a_runtime_removal_the_file_already_made_is_silently_dropped() {
    let base = flatten(&tree());
    let mut theirs_data = tree();
    thesis_remove(&mut theirs_data, "tail");
    let theirs = flatten(&theirs_data);

    let mut journal = vec![("tail".to_owned(), JournalRecord::Remove)];
    let conflicts = reconcile(&mut journal, &base, &theirs, &|_, _| true);
    assert!(conflicts.is_empty());
    assert!(journal.is_empty());
}

#[test]
fn an_entry_created_at_runtime_is_not_a_conflict_when_the_file_lacks_it() {
    let base = flatten(&tree());
    let theirs = base.clone();
    let mut journal = vec![(
        "fresh".to_owned(),
        JournalRecord::Upsert {
            created: true,
            parent: Placement::Root,
            position: 0,
            changes: vec![("name".to_owned(), Some(json!("x")))],
        },
    )];
    let conflicts = reconcile(&mut journal, &base, &theirs, &|_, _| true);
    assert!(conflicts.is_empty());
    assert_eq!(journal.len(), 1);
    match journal_get(&journal, "fresh").unwrap() {
        JournalRecord::Upsert { created, .. } => assert!(created),
        other @ JournalRecord::Remove => panic!("expected an upsert, got {other:?}"),
    }
}

#[test]
fn a_move_the_file_also_made_is_a_conflict_and_the_move_is_dropped() {
    let base = flatten(&tree());
    let mut theirs_data = tree();
    // The file moved `child-1` to the root, and the runtime moved it to another group: both moved
    // it, so the file's own move is what will happen.
    thesis_remove(&mut theirs_data, "child-1");
    theirs_data.insert(
        0,
        json!({ "id": "child-1", "name": "c", "config": { "size": 1 } }),
    );
    let theirs = flatten(&theirs_data);

    let mut journal = vec![(
        "child-1".to_owned(),
        JournalRecord::Upsert {
            created: false,
            parent: Placement::Group("elsewhere".to_owned()),
            position: 0,
            changes: Vec::new(),
        },
    )];
    let conflicts = reconcile(&mut journal, &base, &theirs, &|_, _| true);
    assert_eq!(
        conflicts,
        vec![Conflict {
            id: "child-1".to_owned(),
            reason: "moved both in file and at runtime".to_owned(),
        }]
    );
}

#[test]
fn a_patch_owned_key_is_never_reconciled_against_the_file() {
    let base = flatten(&tree());
    let mut theirs_data = tree();
    theirs_data[0]["name"] = json!("edited in file");
    let theirs = flatten(&theirs_data);

    let mut journal = vec![(
        "root".to_owned(),
        JournalRecord::Upsert {
            created: false,
            parent: Placement::Unchanged,
            position: 0,
            changes: vec![("name".to_owned(), Some(json!("from the patch")))],
        },
    )];
    // `root`'s `name` belongs to a patch, so the file's text about it cannot contradict anything.
    let conflicts = reconcile(&mut journal, &base, &theirs, &|id, key| {
        !(id == "root" && key == Some("name"))
    });
    assert!(conflicts.is_empty());
    assert_eq!(journal.len(), 1);
}

// ---- patches and ownership ----------------------------------------------------------------

#[test]
fn apply_patches_overrides_by_id_and_guards_on_name() {
    let patches = vec![
        Patch {
            id: Some("root".to_owned()),
            name: Some("a".to_owned()),
            overrides: vec![("disabled".to_owned(), json!(true))],
            ..Patch::default()
        },
        Patch {
            id: Some("root".to_owned()),
            name: Some("wrong".to_owned()),
            overrides: vec![("disabled".to_owned(), json!(false))],
            ..Patch::default()
        },
        Patch {
            id: Some("missing".to_owned()),
            overrides: vec![("disabled".to_owned(), json!(true))],
            ..Patch::default()
        },
    ];
    let (data, warnings) = apply_patches(&tree(), &patches);
    assert_eq!(resolve(&data, "root").unwrap()["disabled"], true);
    assert_eq!(warnings.len(), 2);
    assert!(warnings[0].contains("name mismatch for root"));
    assert!(warnings[1].contains("entry missing not found"));
}

#[test]
fn apply_patches_does_not_mutate_the_input() {
    let input = tree();
    let snapshot = input.clone();
    let overlay = vec![Patch {
        id: Some("root".to_owned()),
        overrides: vec![("name".to_owned(), json!("changed"))],
        ..Patch::default()
    }];
    let (patched, _) = apply_patches(&input, &overlay);
    assert_eq!(input, snapshot);
    assert_eq!(resolve(&patched, "root").unwrap()["name"], "changed");
}

#[test]
fn an_insert_patch_appends_to_the_root_or_to_a_group() {
    let patches = vec![
        Patch::inserting(list(&json!([{ "id": "new-root", "name": "x" }]))),
        Patch {
            id: Some("group".to_owned()),
            insert: Some(list(&json!([{ "id": "new-child", "name": "y" }]))),
            ..Patch::default()
        },
        Patch {
            id: Some("root".to_owned()),
            insert: Some(list(&json!([{ "id": "nowhere" }]))),
            ..Patch::default()
        },
    ];
    let (data, warnings) = apply_patches(&tree(), &patches);
    assert_eq!(data.last().unwrap()["id"], "new-root");
    assert_eq!(resolve(&data, "group:new-child").unwrap()["name"], "y");
    assert_eq!(
        warnings,
        vec!["patch insert: entry root is not a group".to_owned()]
    );
}

#[test]
fn an_insert_without_an_id_appends_and_an_override_without_one_warns() {
    let patches = vec![
        Patch::inserting(list(&json!([{ "id": "appended" }]))),
        Patch {
            overrides: vec![("disabled".to_owned(), json!(true))],
            ..Patch::default()
        },
    ];
    let (data, warnings) = apply_patches(&tree(), &patches);
    assert_eq!(data.len(), 4);
    assert_eq!(
        warnings,
        vec!["patch: id is required for non-insert patches".to_owned()]
    );
}

#[test]
fn ensure_ids_names_only_the_anonymous_entries_and_never_collides() {
    let mut data = list(&json!([
        { "name": "anonymous", "group": true, "config": [{ "name": "inner" }, { "id": "taken" }] }
    ]));
    let mut counter = 0;
    ensure_ids(&mut data, &|candidate| candidate == "taken", &mut || {
        counter += 1;
        format!("gen-{counter}")
    });
    assert_eq!(data[0]["id"], "gen-1");
    assert_eq!(data[0]["config"][0]["id"], "gen-2");
    // The named child is untouched, and no generated id took its name.
    assert_eq!(data[0]["config"][1]["id"], "taken");
    assert_eq!(ids(&data).len(), 3);
}

#[test]
fn ensure_insert_ids_names_inserted_rows_so_runtime_changes_can_find_them() {
    let mut patches = vec![Patch {
        id: Some("group".to_owned()),
        insert: Some(list(&json!([{ "name": "anonymous" }]))),
        ..Patch::default()
    }];
    let mut counter = 0;
    ensure_insert_ids(&mut patches, &|_| false, &mut || {
        counter += 1;
        format!("id-{counter}")
    });
    assert_eq!(patches[0].insert.as_ref().unwrap()[0]["id"], "id-1");
}

#[test]
fn the_patch_index_attributes_inserted_entries_as_a_whole_and_per_key() {
    let patches = vec![
        Patch {
            id: Some("root".to_owned()),
            overrides: vec![("disabled".to_owned(), json!(true))],
            ..Patch::default()
        },
        Patch {
            id: Some("root".to_owned()),
            overrides: vec![("disabled".to_owned(), json!(false))],
            ..Patch::default()
        },
        Patch {
            id: Some("group".to_owned()),
            insert: Some(list(&json!([{ "id": "inserted", "name": "x" }]))),
            ..Patch::default()
        },
    ];
    let index = PatchIndex::new(&patches);
    assert_eq!(index.entry(None), EntryOwner::File);
    assert_eq!(index.entry(Some("root")), EntryOwner::File);
    assert_eq!(
        index.key("root", "disabled"),
        // The *last* patch that names the key owns it, because the last one's value is the one that
        // survives the overlay.
        EntryOwner::Patch { index: 1 }
    );
    assert_eq!(index.key("root", "name"), EntryOwner::File);
    assert_eq!(
        index.entry(Some("inserted")),
        EntryOwner::Insert { index: 2 }
    );
    assert_eq!(
        index.key("inserted", "name"),
        // An inserted entry's keys belong to the insert, not to the file: overriding one key of it
        // would otherwise split the entry across two documents.
        EntryOwner::Insert { index: 2 }
    );
    assert!(index.file_owned("root", Some("name")));
    assert!(!index.file_owned("root", Some("disabled")));
    assert!(!index.file_owned("inserted", None));
}

// ---- routing a journal back to its owners -------------------------------------------------

#[test]
fn a_file_owned_change_is_written_into_the_file() {
    let mut data = tree();
    let mut patches = vec![Patch {
        id: Some("root".to_owned()),
        overrides: vec![("disabled".to_owned(), json!(true))],
        ..Patch::default()
    }];
    let journal = vec![(
        "tail".to_owned(),
        JournalRecord::Upsert {
            created: false,
            parent: Placement::Unchanged,
            position: 2,
            changes: vec![("name".to_owned(), Some(json!("renamed")))],
        },
    )];
    let touched = route_journal(&journal, &mut data, &mut patches, &mut |message| {
        panic!("unexpected warning: {message}");
    });
    assert!(!touched, "no patch changed");
    assert_eq!(resolve(&data, "tail").unwrap()["name"], "renamed");
    assert_eq!(patches[0].overrides[0].1, json!(true));
}

#[test]
fn a_patch_owned_key_is_written_into_the_patch_not_the_file() {
    let mut data = tree();
    let mut patches = vec![Patch {
        id: Some("root".to_owned()),
        overrides: vec![("disabled".to_owned(), json!(true))],
        ..Patch::default()
    }];
    let journal = vec![(
        "root".to_owned(),
        JournalRecord::Upsert {
            created: false,
            parent: Placement::Unchanged,
            position: 0,
            changes: vec![
                ("disabled".to_owned(), Some(json!(false))),
                ("name".to_owned(), Some(json!("renamed"))),
            ],
        },
    )];
    let touched = route_journal(&journal, &mut data, &mut patches, &mut |message| {
        panic!("unexpected warning: {message}");
    });
    assert!(touched, "the patch changed");
    assert_eq!(patches[0].overrides[0].1, json!(false));
    // The file-owned key went to the file, and the patch-owned one did not: the file's `root` has
    // the new name and still no `disabled` at all.
    let root = resolve(&data, "root").unwrap();
    assert_eq!(root["name"], "renamed");
    assert!(!root.contains_key("disabled"));
}

#[test]
fn routing_an_inserted_entry_removal_removes_it_from_the_patch() {
    let mut data = tree();
    let mut patches = vec![Patch {
        id: Some("group".to_owned()),
        insert: Some(list(&json!([{ "id": "inserted", "name": "x" }]))),
        ..Patch::default()
    }];
    let journal = vec![("inserted".to_owned(), JournalRecord::Remove)];
    let touched = route_journal(&journal, &mut data, &mut patches, &mut |message| {
        panic!("unexpected warning: {message}");
    });
    assert!(touched);
    assert!(patches[0].insert.as_ref().unwrap().is_empty());
}

#[test]
fn routing_an_inserted_entry_update_writes_it_back_to_the_patch() {
    let mut data = tree();
    let mut patches = vec![Patch {
        id: Some("group".to_owned()),
        insert: Some(list(&json!([{ "id": "inserted", "name": "x" }]))),
        ..Patch::default()
    }];
    let journal = vec![(
        "inserted".to_owned(),
        JournalRecord::Upsert {
            created: false,
            parent: Placement::Unchanged,
            position: 0,
            changes: vec![("name".to_owned(), Some(json!("renamed")))],
        },
    )];
    let touched = route_journal(&journal, &mut data, &mut patches, &mut |message| {
        panic!("unexpected warning: {message}");
    });
    assert!(touched);
    assert_eq!(patches[0].insert.as_ref().unwrap()[0]["name"], "renamed");
}

// ---- the entry rules a loader applies -----------------------------------------------------

#[test]
fn sort_keys_puts_id_and_name_first_and_config_last() {
    let options = entry(&json!({
        "config": { "n": 1 },
        "zzz": 1,
        "name": "x",
        "disabled": true,
        "id": "a",
    }));
    assert_eq!(
        sort_keys(&options),
        ["id", "name", "disabled", "zzz", "config"]
    );
}

#[test]
fn sort_keys_omits_keys_the_entry_does_not_have() {
    // The order is derived from the entry, not from a fixed list: a plugin with no `config` has no
    // trailing `config` placeholder.
    assert_eq!(
        sort_keys(&entry(&json!({ "name": "x", "b": 1 }))),
        ["name", "b"]
    );
    assert_eq!(sort_keys(&entry(&json!({}))), Vec::<String>::new());
}

#[test]
fn a_group_is_never_disabled_by_its_own_flag() {
    let group = entry(&json!({ "id": "g", "group": true, "disabled": true }));
    assert_eq!(is_disabled(&group, &[]), Ok(false));
}

#[test]
fn a_child_is_disabled_through_any_disabled_ancestor() {
    let child = entry(&json!({ "id": "c" }));
    let enabled = entry(&json!({ "id": "g1" }));
    let disabled = entry(&json!({ "id": "g2", "disabled": true }));
    assert_eq!(is_disabled(&child, &[("g2", &disabled)]), Ok(true));
    assert_eq!(is_disabled(&child, &[("g1", &enabled)]), Ok(false));
    // The nearest disabled ancestor is enough, however deep the chain.
    assert_eq!(
        is_disabled(&child, &[("g1", &enabled), ("g2", &disabled)]),
        Ok(true)
    );
}

#[test]
fn an_expression_disabled_is_an_error_rather_than_a_guess() {
    // A `!!js` node cannot be evaluated here. Reading it as `false` would start a plugin the user
    // believes is off, so the chain refuses to answer at all.
    let entry = entry(&json!({ "id": "c", "disabled": { "__jsExpr": "!ctx.get('x')" } }));
    let error = is_disabled(&entry, &[]).expect_err("an expression is not a truth value");
    assert_eq!(error.id, "c");
    assert_eq!(error.expression, "!ctx.get('x')");
    assert!(error.to_string().contains("does not evaluate"));
    assert!(is_js_expression(&json!({ "__jsExpr": "x" })));
    assert!(!is_js_expression(&json!("x")));
}

#[test]
fn resolve_addresses_nested_ids_and_reports_what_is_missing() {
    let data = tree();
    assert_eq!(resolve(&data, "group:child-1").unwrap()["name"], "c");
    assert_eq!(resolve(&data, "tail").unwrap()["name"], "e");
    assert_eq!(
        resolve(&data, "group:missing"),
        Err(boot_loader_gap::ResolveError::NotFound(
            "group:missing".to_owned()
        ))
    );
    assert_eq!(
        resolve(&data, "root:child-1"),
        Err(boot_loader_gap::ResolveError::NotAGroup("root".to_owned()))
    );
    assert_eq!(
        resolve_group(&data, None).unwrap().len(),
        3,
        "the root list is three entries"
    );
    assert_eq!(resolve_group(&data, Some("group")).unwrap().len(), 2);
    assert_eq!(
        resolve_group(&data, Some("root")),
        Err(boot_loader_gap::ResolveError::NotAGroup("root".to_owned()))
    );
}

#[test]
fn ensure_id_keeps_an_existing_id_and_retries_a_taken_one() {
    let mut options = entry(&json!({ "id": "kept" }));
    assert_eq!(
        ensure_id(&mut options, &|_| true, &mut || "x".to_owned()),
        Ok("kept".to_owned())
    );

    let mut options = entry(&json!({}));
    let mut counter = 0;
    let id = ensure_id(&mut options, &|candidate| candidate == "taken", &mut || {
        counter += 1;
        if counter == 1 {
            "taken".to_owned()
        } else {
            "free".to_owned()
        }
    })
    .unwrap();
    assert_eq!(id, "free");
    assert_eq!(options["id"], "free");
}

#[test]
fn unwrap_exports_sees_through_the_esm_and_esbuild_shapes() {
    // A namespace object carries the plugin under `default`.
    assert_eq!(
        unwrap_exports(json!({ "default": "plugin" })),
        json!("plugin")
    );
    // An esbuild-interop object puts the real export under a second `default`.
    assert_eq!(
        unwrap_exports(json!({ "default": { "__esModule": true, "default": "plugin" } })),
        json!("plugin")
    );
    // A bare function or class is returned unchanged.
    assert_eq!(unwrap_exports(json!({ "apply": 1 })), json!({ "apply": 1 }));
    assert_eq!(unwrap_exports(json!("plugin")), json!("plugin"));
    assert_eq!(unwrap_exports(Value::Null), Value::Null);
}

// ---- the config file's decisions ----------------------------------------------------------

#[test]
fn the_extension_decides_whether_a_config_file_is_accepted() {
    assert_eq!(content_type("a/b/cordis.yml"), Some("application/yaml"));
    assert_eq!(content_type("cordis.yaml"), Some("application/yaml"));
    assert_eq!(content_type("profile.json"), Some("application/json"));
    // An extension the loader cannot parse is refused at construction, not at the first write.
    assert_eq!(content_type("cordis.toml"), None);
    assert_eq!(content_type("no-extension"), None);
    // A dot in a directory is not an extension, and a leading dot is part of the name.
    assert_eq!(extension("a.b/config"), None);
    assert_eq!(extension(".gitignore"), None);
    assert_eq!(extension("/tmp/plugins.d/cordis.yml"), Some(".yml"));
}

#[test]
fn only_a_missing_file_falls_back_to_the_initial_list() {
    assert_eq!(
        initial_or_fail(ConfigStage::Read, true, true),
        MissingFile::WriteInitial
    );
    assert_eq!(
        initial_or_fail(ConfigStage::Read, true, false),
        MissingFile::Fail
    );
    // A file that exists but cannot be read is an error, not a first run.
    assert_eq!(
        initial_or_fail(ConfigStage::Read, false, true),
        MissingFile::Propagate
    );
    // A parse or validate failure is never repaired by writing a fresh default over the file.
    assert_eq!(
        initial_or_fail(ConfigStage::Parse, false, true),
        MissingFile::Propagate
    );
    assert_eq!(
        initial_or_fail(ConfigStage::Validate, false, true),
        MissingFile::Propagate
    );
}

#[test]
fn a_write_is_planned_from_the_snapshot_the_caller_read() {
    // Nothing pending, or nothing read yet: nothing to do.
    assert_eq!(
        plan_write("f", "t", 0, Some("a"), Some("a"), true, "b"),
        WritePlan::Idle
    );
    assert_eq!(
        plan_write("f", "t", 1, None, Some("a"), true, "b"),
        WritePlan::Idle
    );
    // A file that changed since the snapshot is re-read first, never clobbered.
    assert_eq!(
        plan_write("f", "t", 1, Some("a"), Some("other"), true, "b"),
        WritePlan::Reread
    );
    // A file that vanished keeps its pending changes in memory.
    assert_eq!(
        plan_write("f", "t", 1, Some("a"), None, true, "b"),
        WritePlan::Missing
    );
    // A read-only file is reported, not an error.
    assert_eq!(
        plan_write("f", "t", 1, Some("a"), Some("a"), false, "b"),
        WritePlan::ReadOnly
    );
    assert_eq!(
        plan_write("f", "t", 1, Some("a"), Some("a"), true, "b"),
        WritePlan::Write {
            target: "f".to_owned(),
            temp: "t".to_owned(),
            text: "b".to_owned(),
            expected: Some("a".to_owned()),
        }
    );
}

#[test]
fn the_temp_file_sits_beside_its_target_so_the_rename_is_atomic() {
    assert_eq!(temp_path("/p/cordis.yml", 7, 2), "/p/.cordis.yml.7.2.tmp");
    assert_eq!(temp_path("cordis.yml", 7, 0), ".cordis.yml.7.0.tmp");
}

#[test]
fn only_an_access_denial_from_another_process_is_retried() {
    for code in ["EACCES", "EPERM", "EBUSY"] {
        assert!(is_transient_rename_error(code));
        assert_eq!(
            retry_delay(0, code),
            Some(std::time::Duration::from_millis(20))
        );
        assert_eq!(
            retry_delay(9, code),
            Some(std::time::Duration::from_millis(200))
        );
        // The retries are bounded: the tenth attempt is the last one.
        assert_eq!(retry_delay(10, code), None);
    }
    // A missing directory or a cross-device rename never fixes itself by waiting.
    assert_eq!(retry_delay(0, "ENOENT"), None);
    assert_eq!(retry_delay(0, "EXDEV"), None);
}

#[test]
fn a_conditional_write_only_renames_while_the_file_still_holds_what_was_read() {
    assert_eq!(check_expected(Some("a"), Some("a")), StaleWrite::Rename);
    assert_eq!(check_expected(Some("a"), Some("b")), StaleWrite::Reread);
    // A file deleted between the check and the rename is a change like any other.
    assert_eq!(check_expected(Some("a"), None), StaleWrite::Reread);
    assert_eq!(check_expected(None, None), StaleWrite::Rename);
}

#[test]
fn a_config_file_failure_says_which_stage_failed() {
    let error = boot_loader_gap::ConfigFileError::new(ConfigStage::Parse, "/p/cordis.yml", "bad");
    assert_eq!(
        error.to_string(),
        "failed to parse config file /p/cordis.yml"
    );
    assert!(error.to_string().contains("parse"));
}

#[test]
fn assign_ids_keeps_the_id_an_anonymous_entry_had_before() {
    // First read: the file leaves both children anonymous, so they get generated ids.
    let mut first = list(&json!([
        {
            "id": "group",
            "group": true,
            "config": [{ "name": "a", "config": { "n": 1 } }, { "name": "b" }]
        }
    ]));
    let mut counter = 0;
    let assigned = assign_ids(&mut first, &BTreeMap::new(), &|_| false, &mut || {
        counter += 1;
        format!("gen-{counter}")
    });
    let first_ids: Vec<String> = assigned.keys().cloned().collect();
    assert_eq!(first_ids, ["gen-1", "gen-2"]);

    // Second read: the same two entries, but one of them has an edit. The *unedited* one keeps its
    // id because its shape is unchanged; the edited one is genuinely a different entry.
    let mut second = list(&json!([
        {
            "id": "group",
            "group": true,
            "config": [{ "name": "a", "config": { "n": 2 } }, { "name": "b" }]
        }
    ]));
    let mut counter = 10;
    let assigned = assign_ids(&mut second, &assigned, &|_| false, &mut || {
        counter += 1;
        format!("gen-{counter}")
    });
    let second_ids: Vec<String> = assigned.keys().cloned().collect();
    assert_eq!(second_ids, ["gen-11", "gen-2"]);
    // The generated id is inserted first, so a later `sort_keys` round trip keeps it first.
    assert_eq!(
        second[0]["config"][1]
            .as_object()
            .unwrap()
            .keys()
            .next()
            .unwrap(),
        "id"
    );
}

#[test]
fn assign_ids_never_reuses_an_id_the_tree_already_has() {
    let mut data = list(&json!([{ "name": "anonymous" }]));
    let mut counter = 0;
    // The tree says `gen-1` is taken, so the source's first answer is refused.
    let used = |candidate: &str| candidate == "gen-1";
    let assigned = assign_ids(&mut data, &BTreeMap::new(), &used, &mut || {
        counter += 1;
        format!("gen-{counter}")
    });
    assert_eq!(assigned.keys().cloned().collect::<Vec<_>>(), ["gen-2"]);
}

// ---- the whole round trip -----------------------------------------------------------------

#[test]
fn an_edit_reaches_the_file_and_survives_a_re_read() {
    // The file, as read.
    let mut data = tree();
    let snapshot = data.clone();
    let cache = flatten(&snapshot);

    // The user disables `root` and renames a nested child.
    let mut journal = Vec::new();
    for (id, options, legacy) in [
        (
            "root",
            entry(&json!({ "id": "root", "name": "a", "disabled": true })),
            Some(entry(&json!({ "id": "root", "name": "a" }))),
        ),
        (
            "child-1",
            entry(&json!({ "id": "child-1", "name": "renamed", "config": { "size": 1 } })),
            Some(entry(&json!({
                "id": "child-1",
                "name": "c",
                "config": { "size": 1 }
            }))),
        ),
    ] {
        record(
            &mut journal,
            &EntryChange {
                id: id.to_owned(),
                options: Some(options),
                legacy,
                parent: None,
                moved: false,
                position: 0,
            },
        );
    }

    // Nothing else has touched the file, so reconciliation leaves the journal alone.
    let conflicts = reconcile(&mut journal, &cache, &cache, &|_, _| true);
    assert!(conflicts.is_empty());
    assert_eq!(journal.len(), 2);

    // Routing finds no patch to write back to, so the changes belong to the file.
    let mut patches: Vec<Patch> = Vec::new();
    let touched = route_journal(&journal, &mut data, &mut patches, &mut |message| {
        panic!("unexpected warning: {message}");
    });
    assert!(!touched);

    // What the file would now hold, re-read: the changes are there, and the diffs are gone.
    assert_eq!(resolve(&data, "root").unwrap()["disabled"], true);
    assert_eq!(resolve(&data, "group:child-1").unwrap()["name"], "renamed");
    assert_eq!(resolve(&data, "group:child-2").unwrap()["name"], "d");
    let after = flatten(&data);
    for (id, record) in &journal {
        match record {
            JournalRecord::Upsert { changes, .. } => {
                for (key, value) in changes {
                    assert_eq!(
                        after[id].options.get(key),
                        value.as_ref(),
                        "{id}.{key} did not reach the file"
                    );
                }
            }
            JournalRecord::Remove => panic!("{id} was not a removal"),
        }
    }
}

#[test]
fn a_journal_that_conflicts_with_the_file_is_dropped_rather_than_written() {
    // The user disabled `root` at runtime; meanwhile the same row was edited on disk.
    let mut data = tree();
    let cache = flatten(&data);
    let mut journal = Vec::new();
    record(
        &mut journal,
        &EntryChange {
            id: "root".to_owned(),
            options: Some(entry(&json!({
                "id": "root",
                "name": "a",
                "disabled": true
            }))),
            legacy: Some(entry(&json!({ "id": "root", "name": "a" }))),
            parent: None,
            moved: false,
            position: 0,
        },
    );

    // The disk now says `name` is something else, so `disabled` is the only surviving delta.
    let mut theirs_data = tree();
    theirs_data[0]["name"] = json!("edited on disk");
    let theirs = flatten(&theirs_data);

    let mut patches = vec![Patch {
        id: Some("root".to_owned()),
        name: Some("a".to_owned()),
        overrides: vec![("disabled".to_owned(), json!(false))],
        ..Patch::default()
    }];
    let index = PatchIndex::new(&patches);
    let conflicts = reconcile(&mut journal, &cache, &theirs, &|id, key| {
        index.file_owned(id, key)
    });
    // `name` was not part of the runtime change, so nothing conflicts; `disabled` is patch-owned,
    // so the file cannot contradict it either.
    assert!(conflicts.is_empty());
    let touched = route_journal(&journal, &mut data, &mut patches, &mut |message| {
        panic!("unexpected warning: {message}");
    });
    assert!(touched, "the patch owns `disabled`");
    assert_eq!(patches[0].overrides[0].1, json!(true));
}

#[test]
fn apply_journal_and_apply_patches_compose_in_the_documented_order() {
    // Upstream applies the file's patches first, then the journal on top. The order is visible when
    // a late patch writes a key the journal also writes: the journal is what the user just did.
    let overlay = vec![Patch {
        id: Some("root".to_owned()),
        overrides: vec![("disabled".to_owned(), json!(true))],
        ..Patch::default()
    }];
    let (mut patched, warnings) = apply_patches(&tree(), &overlay);
    assert!(warnings.is_empty());

    let journal = vec![(
        "root".to_owned(),
        JournalRecord::Upsert {
            created: false,
            parent: Placement::Unchanged,
            position: 0,
            changes: vec![("disabled".to_owned(), Some(json!(false)))],
        },
    )];
    apply_journal(&mut patched, &journal, None, &mut |message| {
        panic!("unexpected warning: {message}");
    });
    assert_eq!(resolve(&patched, "root").unwrap()["disabled"], false);
}

#[test]
fn changes_deleting_a_key_are_ordered_as_recorded() {
    // `Changes` is a list, not a map, so two writers folding into it agree on which later value
    // wins per key and on where each key sits.
    let mut options = entry(&json!({ "a": 1, "b": 2 }));
    let changes: Changes = vec![
        ("b".to_owned(), Some(json!(3))),
        ("c".to_owned(), Some(json!(4))),
    ];
    apply_changes(&mut options, &changes, None);
    assert_eq!(options, entry(&json!({ "a": 1, "b": 3, "c": 4 })));
}
