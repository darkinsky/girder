//! Tests for the workspace and settings surfaces.
//!
//! The workspace registry's interesting part is the manual order: a user who drags one workspace above
//! another expects it to stay there, and an off-by-one when reordering is invisible until they look.

use girder_core::api_config::{
    SettingView, SettingsDescribeValue, SettingsUpdateRequest, WorkspaceCreateRequest,
    WorkspaceDeleteValue, WorkspaceRegistry, WorkspaceValue, WorkspaceView, config_catalogue,
    settings_methods, workspace_methods,
};
use girder_core::{Schema, to_typescript};

// ---- wire types -------------------------------------------------------------------------------

#[test]
fn a_workspace_view_lists_its_sessions_in_order() {
    // Manual rather than by creation time: a dragged session must stay where it was put.
    let schema = WorkspaceView::typert_schema();

    assert_eq!(
        schema.fields[3].schema,
        Schema::Array(Box::new(Schema::String))
    );
    assert_eq!(schema.fields[3].name, "session_ids");
}

#[test]
fn a_create_request_makes_the_title_optional() {
    // Omitted derives one from the directory name, so a client need not invent one.
    let schema = WorkspaceCreateRequest::typert_schema();

    assert_eq!(schema.required_at(1), vec!["path"]);
}

#[test]
fn a_delete_value_confirms_without_describing() {
    let schema = WorkspaceDeleteValue::typert_schema();

    assert_eq!(schema.fields[0].name, "deleted");
    assert_eq!(schema.fields[0].schema, Schema::Bool);
}

#[test]
fn a_setting_view_carries_its_editability() {
    // False for a value the deployment pins, such as an endpoint derived from the environment.
    let schema = SettingView::typert_schema();
    let editable = schema
        .fields
        .iter()
        .find(|field| field.name == "editable")
        .expect("editable exists");

    assert_eq!(editable.schema, Schema::Bool);
    assert!(editable.required);
}

#[test]
fn a_setting_value_is_opaque() {
    // The type name tells the client how to render it; the schema does not enumerate every shape.
    let schema = SettingsUpdateRequest::typert_schema();
    let value = schema
        .fields
        .iter()
        .find(|field| field.name == "value")
        .expect("value exists");

    assert_eq!(value.schema, Schema::Any);
}

#[test]
fn a_describe_value_is_a_list_of_views() {
    let schema = SettingsDescribeValue::typert_schema();

    assert_eq!(
        schema.fields[0].schema,
        Schema::Array(Box::new(Schema::Ref("SettingView".to_owned())))
    );
}

// ---- methods ----------------------------------------------------------------------------------

#[test]
fn the_workspace_methods_are_namespaced() {
    for method in workspace_methods() {
        assert_eq!(method.namespace, "workspace", "{}", method.name);
    }
}

#[test]
fn the_settings_methods_are_namespaced() {
    for method in settings_methods() {
        assert_eq!(method.namespace, "settings", "{}", method.name);
    }
}

#[test]
fn the_catalogue_defines_every_referenced_type() {
    let catalogue = config_catalogue();

    for method in &catalogue.methods {
        if let Schema::Ref(name) = &method.returns {
            assert!(
                catalogue.types.contains_key(name),
                "`{}` returns an undefined `{name}`",
                method.qualified()
            );
        }
    }
}

#[test]
fn the_catalogue_renders_valid_typescript() {
    let output = to_typescript(&config_catalogue());

    assert_eq!(
        output.chars().filter(|c| *c == '{').count(),
        output.chars().filter(|c| *c == '}').count()
    );
    assert!(output.contains("'workspace.list': {"), "{output}");
    assert!(output.contains("'settings.describe': {"), "{output}");
}

// ---- the registry -----------------------------------------------------------------------------

fn registry() -> WorkspaceRegistry {
    WorkspaceRegistry::new()
}

#[test]
fn registering_a_directory_creates_a_workspace() {
    let registry = registry();
    let workspace = registry
        .register("w1", "/projects/alpha", None, 1000)
        .expect("registered");

    assert_eq!(workspace.id, "w1");
    assert_eq!(workspace.title, "alpha", "derived from the directory name");
    assert!(workspace.session_ids.is_empty());
}

#[test]
fn an_explicit_title_wins() {
    let registry = registry();
    let workspace = registry
        .register("w1", "/projects/alpha", Some("My Project".to_owned()), 1000)
        .expect("registered");

    assert_eq!(workspace.title, "My Project");
}

#[test]
fn registering_the_same_path_twice_is_refused() {
    // Two entries for one directory means a client shows the same project twice.
    let registry = registry();
    registry
        .register("w1", "/projects/alpha", None, 1000)
        .expect("registered");

    let error = registry
        .register("w2", "/projects/alpha", None, 2000)
        .expect_err("already registered");

    assert_eq!(error, "w1", "the error names the existing entry");
}

#[test]
fn the_list_preserves_manual_order() {
    let registry = registry();
    for (id, path) in [("a", "/a"), ("b", "/b"), ("c", "/c")] {
        registry.register(id, path, None, 1000).expect("registered");
    }

    let ids: Vec<String> = registry.list().into_iter().map(|w| w.id).collect();

    assert_eq!(ids, vec!["a", "b", "c"]);
}

#[test]
fn renaming_updates_the_title_and_the_timestamp() {
    let registry = registry();
    registry
        .register("w1", "/a", None, 1000)
        .expect("registered");

    let renamed = registry.rename("w1", "Renamed", 2000).expect("renamed");

    assert_eq!(renamed.title, "Renamed");
    assert_eq!(renamed.updated_at, 2000);
    assert_eq!(renamed.created_at, 1000, "creation time does not move");
}

#[test]
fn renaming_an_unknown_workspace_returns_nothing() {
    assert!(registry().rename("nope", "x", 1000).is_none());
}

#[test]
fn deleting_removes_the_registration_only() {
    // Closing a tab, not deleting a project: the directory is untouched.
    let registry = registry();
    registry
        .register("w1", "/a", None, 1000)
        .expect("registered");

    assert!(registry.delete("w1"));
    assert!(registry.list().is_empty());
    assert!(
        !registry.delete("w1"),
        "a second delete reports nothing removed"
    );
}

#[test]
fn moving_forward_places_the_workspace_before_the_target() {
    let registry = registry();
    for (id, path) in [("a", "/a"), ("b", "/b"), ("c", "/c")] {
        registry.register(id, path, None, 1000).expect("registered");
    }

    // Move `c` before `a`: the result is c, a, b.
    let ordered = registry.insert_before("c", "a", 2000).expect("moved");
    let ids: Vec<&str> = ordered.iter().map(|w| w.id.as_str()).collect();

    assert_eq!(ids, vec!["c", "a", "b"]);
}

#[test]
fn moving_backward_accounts_for_the_removal_shift() {
    // The off-by-one this test exists for: removing an earlier element shifts the target left.
    let registry = registry();
    for (id, path) in [("a", "/a"), ("b", "/b"), ("c", "/c"), ("d", "/d")] {
        registry.register(id, path, None, 1000).expect("registered");
    }

    // Move `a` before `c`: removing `a` shifts `c` from index 2 to 1, so it lands at index 1.
    let ordered = registry.insert_before("a", "c", 2000).expect("moved");
    let ids: Vec<&str> = ordered.iter().map(|w| w.id.as_str()).collect();

    assert_eq!(ids, vec!["b", "a", "c", "d"]);
}

#[test]
fn moving_to_its_own_position_is_a_no_op() {
    let registry = registry();
    for (id, path) in [("a", "/a"), ("b", "/b")] {
        registry.register(id, path, None, 1000).expect("registered");
    }

    let ordered = registry.insert_before("a", "a", 2000).expect("moved");
    let ids: Vec<&str> = ordered.iter().map(|w| w.id.as_str()).collect();

    assert_eq!(ids, vec!["a", "b"]);
}

#[test]
fn moving_an_unknown_workspace_returns_nothing() {
    let registry = registry();
    registry
        .register("a", "/a", None, 1000)
        .expect("registered");

    assert!(registry.insert_before("nope", "a", 2000).is_none());
    assert!(registry.insert_before("a", "nope", 2000).is_none());
}

#[test]
fn attaching_adds_a_session_to_its_workspace() {
    let registry = registry();
    registry
        .register("w1", "/a", None, 1000)
        .expect("registered");

    let attached = registry.attach_session("w1", "s1", 1500).expect("attached");

    assert_eq!(attached.session_ids, vec!["s1"]);
    assert_eq!(attached.updated_at, 1500);
}

#[test]
fn attaching_the_same_session_twice_is_idempotent() {
    // A duplicate renders twice in a sidebar and makes a drag reorder ambiguous.
    let registry = registry();
    registry
        .register("w1", "/a", None, 1000)
        .expect("registered");
    registry.attach_session("w1", "s1", 1500).expect("attached");

    let attached = registry
        .attach_session("w1", "s1", 1600)
        .expect("attached again");

    assert_eq!(attached.session_ids, vec!["s1"]);
    assert_eq!(attached.updated_at, 1600, "the timestamp still moves");
}

#[test]
fn archiving_removes_a_session_from_its_workspace() {
    let registry = registry();
    registry
        .register("w1", "/a", None, 1000)
        .expect("registered");
    registry.attach_session("w1", "s1", 1500).expect("attached");

    let archived = registry
        .archive_session("w1", "s1", 2000)
        .expect("archived");

    assert!(archived.session_ids.is_empty());
    assert_eq!(archived.updated_at, 2000);
}

#[test]
fn archiving_an_unattached_session_returns_nothing() {
    // Distinguishable from success, so a client does not report a no-op as an archive.
    let registry = registry();
    registry
        .register("w1", "/a", None, 1000)
        .expect("registered");

    assert!(registry.archive_session("w1", "s1", 2000).is_none());
}

#[test]
fn archiving_from_an_unknown_workspace_returns_nothing() {
    assert!(registry().archive_session("nope", "s1", 1000).is_none());
}

#[test]
fn a_workspace_projects_to_a_view() {
    let registry = registry();
    registry
        .register("w1", "/projects/alpha", Some("Alpha".to_owned()), 1000)
        .expect("registered");

    let view = registry.get("w1").expect("present").view(2000);

    assert_eq!(view.workspace_id, "w1");
    assert_eq!(view.path, "/projects/alpha");
    assert_eq!(view.title, "Alpha");
    assert_eq!(view.created_at, 1000);
}

#[test]
fn a_workspace_value_wraps_one_view() {
    let registry = registry();
    let workspace = registry
        .register("w1", "/a", None, 1000)
        .expect("registered");

    let value = WorkspaceValue {
        workspace: workspace.view(1000),
    };

    assert_eq!(value.workspace.workspace_id, "w1");
}
