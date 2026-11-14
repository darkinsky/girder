//! Tests for schema-default completion of tool-call arguments.
//!
//! The property under test is not "completion adds values" — a function that fills every missing
//! parameter would pass that. It is that completion adds **only** what the schema author wrote down.
//! So the load-bearing tests are the ones that assert a value is still *missing* afterwards: an
//! optional parameter with no default, a required parameter with no default, and a nested object the
//! call omitted. Each of those would be silently papered over by zero-value completion, and each is
//! a case where a filled value would make the model's omission invisible.
//!
//! The second cluster covers [`Provenance`], because the classification is what a caller uses to
//! word an error. A test that only checks the filled arguments would pass even if every parameter
//! were reported `Provided`, which is the exact conflation this module exists to prevent.
//!
//! The module is included by path rather than through the crate root, so these tests compile against
//! it before `lib.rs` declares it, and nothing here touches the rest of `girder_core`.

#[path = "../src/argument_infer.rs"]
mod argument_infer;

use argument_infer::{Provenance, complete_from_defaults, provenance};
use serde_json::{Value, json};

/// The filled-argument paths, sorted for order-insensitive comparison.
fn filled_paths(schema: &Value, args: &Value) -> Vec<String> {
    let mut args = args.clone();
    let mut filled = complete_from_defaults(schema, &mut args).filled;
    filled.sort();
    filled
}

/// The provenance map, keyed by parameter name.
fn origins(schema: &Value, args: &Value) -> Vec<(String, Provenance)> {
    provenance(schema, args)
}

#[test]
fn fills_a_missing_parameter_from_its_default() {
    let schema = json!({
        "type": "object",
        "properties": {
            "path": { "type": "string" },
            "encoding": { "type": "string", "default": "utf8" }
        },
        "required": ["path"]
    });
    let mut args = json!({ "path": "a.txt" });

    let completion = complete_from_defaults(&schema, &mut args);

    assert_eq!(
        completion.arguments,
        json!({ "path": "a.txt", "encoding": "utf8" })
    );
    assert_eq!(
        args, completion.arguments,
        "the &mut value is the reported value"
    );
    assert_eq!(completion.filled, vec!["encoding".to_owned()]);
    assert!(!completion.is_empty());
}

#[test]
fn fills_every_json_kind_of_default_verbatim() {
    // A default is data, not a type: `null`, `false`, `0`, `""` and `[]` are all declarations and
    // must survive unchanged rather than being read as "no default".
    let schema = json!({
        "type": "object",
        "properties": {
            "n": { "type": "integer", "default": 0 },
            "empty": { "type": "string", "default": "" },
            "flag": { "type": "boolean", "default": false },
            "nothing": { "type": "null", "default": null },
            "list": { "type": "array", "default": [] },
            "obj": { "type": "object", "default": { "a": 1 } }
        }
    });
    let mut args = json!({});

    let completion = complete_from_defaults(&schema, &mut args);

    assert_eq!(
        completion.arguments,
        json!({ "n": 0, "empty": "", "flag": false, "nothing": null, "list": [], "obj": { "a": 1 } })
    );
    assert_eq!(completion.filled.len(), 6);
}

#[test]
fn leaves_an_optional_parameter_without_a_default_absent() {
    // The core negative case. Filling `encoding` with `""` or null would validate and would be a
    // lie: "not provided" is not "provided empty".
    let schema = json!({
        "type": "object",
        "properties": {
            "path": { "type": "string" },
            "encoding": { "type": "string" },
            "mode": { "type": "string", "enum": ["ro", "rw"] }
        },
        "required": ["path"]
    });
    let mut args = json!({ "path": "a.txt" });

    let completion = complete_from_defaults(&schema, &mut args);

    assert_eq!(completion.arguments, json!({ "path": "a.txt" }));
    assert!(
        completion.is_empty(),
        "nothing was declared, so nothing may be filled: {:?}",
        completion.filled
    );
    let object = completion
        .arguments
        .as_object()
        .expect("the arguments stay an object");
    assert!(!object.contains_key("encoding"));
    assert!(!object.contains_key("mode"));
}

#[test]
fn does_not_fill_a_required_parameter_that_has_no_default() {
    // `required` is the validator's judgement, not this module's. Inventing a value here would make
    // a call that omitted a required argument look valid, and the model would never learn.
    let schema = json!({
        "type": "object",
        "properties": {
            "path": { "type": "string" },
            "encoding": { "type": "string", "default": "utf8" }
        },
        "required": ["path", "encoding"]
    });
    let mut args = json!({});

    let completion = complete_from_defaults(&schema, &mut args);

    assert!(
        !completion
            .arguments
            .as_object()
            .is_some_and(|o| o.contains_key("path")),
        "`path` is required with no default; it must stay missing"
    );
    assert_eq!(completion.arguments, json!({ "encoding": "utf8" }));
    assert_eq!(completion.filled, vec!["encoding".to_owned()]);
}

#[test]
fn fills_a_required_parameter_that_declares_a_default() {
    // The one case where the two rules meet. The schema author defined the value, so withholding it
    // would fail a call the schema itself says is well-defined. Provenance still records that the
    // model did not send it.
    let schema = json!({
        "type": "object",
        "properties": { "limit": { "type": "integer", "default": 10 } },
        "required": ["limit"]
    });
    let mut args = json!({});

    // Provenance is asked of the call as the model sent it; completion would erase the distinction.
    let origins = origins(&schema, &args);
    let completion = complete_from_defaults(&schema, &mut args);

    assert_eq!(completion.arguments, json!({ "limit": 10 }));
    assert_eq!(origins, vec![("limit".to_owned(), Provenance::Defaulted)]);
}

#[test]
fn an_explicit_value_wins_over_the_default() {
    let schema = json!({
        "type": "object",
        "properties": { "encoding": { "type": "string", "default": "utf8" } }
    });
    let mut args = json!({ "encoding": "latin1" });

    let completion = complete_from_defaults(&schema, &mut args);

    assert_eq!(completion.arguments, json!({ "encoding": "latin1" }));
    assert!(completion.is_empty());
    assert_eq!(
        origins(&schema, &args),
        vec![("encoding".to_owned(), Provenance::Provided)]
    );
}

#[test]
fn an_explicit_null_is_provided_and_is_not_replaced() {
    // A model that sends `"encoding": null` asked for null. Treating it as absent would overwrite a
    // deliberate choice with the schema's value.
    let schema = json!({
        "type": "object",
        "properties": { "encoding": { "type": ["string", "null"], "default": "utf8" } }
    });
    let mut args = json!({ "encoding": null });

    let completion = complete_from_defaults(&schema, &mut args);

    assert_eq!(completion.arguments, json!({ "encoding": null }));
    assert!(completion.is_empty());
    assert_eq!(
        origins(&schema, &args),
        vec![("encoding".to_owned(), Provenance::Provided)]
    );
}

#[test]
fn fills_defaults_inside_a_nested_object_the_call_supplied() {
    let schema = json!({
        "type": "object",
        "properties": {
            "options": {
                "type": "object",
                "properties": {
                    "mode": { "type": "string", "default": "ro" },
                    "depth": {
                        "type": "object",
                        "properties": { "max": { "type": "integer", "default": 3 } }
                    }
                }
            }
        }
    });
    let mut args = json!({ "options": { "depth": {} } });

    let completion = complete_from_defaults(&schema, &mut args);

    assert_eq!(
        completion.arguments,
        json!({ "options": { "mode": "ro", "depth": { "max": 3 } } })
    );
    let mut filled = completion.filled;
    filled.sort();
    assert_eq!(
        filled,
        vec!["options.depth.max".to_owned(), "options.mode".to_owned()]
    );
}

#[test]
fn does_not_create_a_nested_object_the_call_omitted() {
    // Even though `options.mode` has a default, `options` itself has none. Materializing the parent
    // would send an argument group the model never mentioned, which is the empty-value mistake one
    // level up.
    let schema = json!({
        "type": "object",
        "properties": {
            "options": {
                "type": "object",
                "properties": { "mode": { "type": "string", "default": "ro" } }
            }
        }
    });
    let mut args = json!({});

    let completion = complete_from_defaults(&schema, &mut args);

    assert_eq!(completion.arguments, json!({}));
    assert!(completion.is_empty());
}

#[test]
fn a_nested_object_with_its_own_default_is_filled_whole() {
    // The parent *is* declared, so it is filled verbatim; its sub-defaults are then the caller's
    // supplied value to complete, and must not be merged into the author's literal.
    let schema = json!({
        "type": "object",
        "properties": {
            "options": {
                "type": "object",
                "default": { "mode": "ro" },
                "properties": {
                    "mode": { "type": "string", "default": "rw" },
                    "depth": { "type": "integer", "default": 2 }
                }
            }
        }
    });
    let mut args = json!({});

    let completion = complete_from_defaults(&schema, &mut args);

    assert_eq!(
        completion.arguments,
        json!({ "options": { "mode": "ro", "depth": 2 } })
    );
    let mut filled = completion.filled;
    filled.sort();
    assert_eq!(
        filled,
        vec!["options".to_owned(), "options.depth".to_owned()]
    );
}

#[test]
fn does_not_descend_into_array_elements() {
    // Array length is the call's decision. Filling a default inside an element would add fields to
    // values the model wrote, and the element count is not this module's to reinterpret.
    let schema = json!({
        "type": "object",
        "properties": {
            "edits": {
                "type": "array",
                "items": {
                    "type": "object",
                    "properties": { "mode": { "type": "string", "default": "replace" } }
                }
            }
        }
    });
    let mut args = json!({ "edits": [{ "path": "a.txt" }] });

    let completion = complete_from_defaults(&schema, &mut args);

    assert_eq!(
        completion.arguments,
        json!({ "edits": [{ "path": "a.txt" }] })
    );
    assert!(completion.is_empty());
}

#[test]
fn a_schema_without_defaults_fills_nothing() {
    let schema = json!({
        "type": "object",
        "properties": {
            "a": { "type": "string" },
            "b": { "type": "integer" }
        },
        "required": ["a"]
    });
    let mut args = json!({ "a": "x" });

    let completion = complete_from_defaults(&schema, &mut args);

    assert_eq!(completion.arguments, json!({ "a": "x" }));
    assert!(completion.is_empty());
}

#[test]
fn an_empty_call_gains_only_the_declared_defaults() {
    // "Empty arguments" is the case where zero-value completion would be most damaging: the whole
    // call would silently acquire an empty string for every string parameter.
    let schema = json!({
        "type": "object",
        "properties": {
            "path": { "type": "string" },
            "encoding": { "type": "string", "default": "utf8" },
            "recursive": { "type": "boolean", "default": false }
        },
        "required": ["path"]
    });
    let mut args = json!({});

    let completion = complete_from_defaults(&schema, &mut args);

    assert_eq!(
        completion.arguments,
        json!({ "encoding": "utf8", "recursive": false })
    );
}

#[test]
fn an_empty_schema_object_fills_nothing() {
    let schema = json!({});
    let mut args = json!({ "a": 1 });

    let completion = complete_from_defaults(&schema, &mut args);

    assert_eq!(completion.arguments, json!({ "a": 1 }));
    assert!(completion.is_empty());
    assert!(origins(&schema, &args).is_empty());
}

#[test]
fn provenance_reports_all_three_origins() {
    let schema = json!({
        "type": "object",
        "properties": {
            "given": { "type": "string" },
            "fallback": { "type": "string", "default": "utf8" },
            "optional": { "type": "string" }
        },
        "required": ["given"]
    });
    let mut args = json!({ "given": "x" });
    let origins = origins(&schema, &args);

    let completion = complete_from_defaults(&schema, &mut args);
    assert_eq!(
        completion.arguments,
        json!({ "given": "x", "fallback": "utf8" })
    );

    assert_eq!(
        origins,
        vec![
            ("fallback".to_owned(), Provenance::Defaulted),
            ("given".to_owned(), Provenance::Provided),
            ("optional".to_owned(), Provenance::Absent),
        ],
        "properties are reported in schema (key) order"
    );
}

#[test]
fn provenance_distinguishes_an_empty_string_from_an_omission() {
    // The distinction that decides error wording: `""` is a value the model chose and must be
    // reported as provided, while a missing key must not be confused with it.
    let schema = json!({
        "type": "object",
        "properties": { "note": { "type": "string" } }
    });

    let sent_empty = json!({ "note": "" });
    let omitted = json!({});

    assert_eq!(
        origins(&schema, &sent_empty),
        vec![("note".to_owned(), Provenance::Provided)]
    );
    assert_eq!(
        origins(&schema, &omitted),
        vec![("note".to_owned(), Provenance::Absent)]
    );
}

#[test]
fn provenance_reports_a_required_name_absent_from_properties() {
    // A `required` entry with no matching property is an authoring mistake. Reporting it as `Absent`
    // shows the caller the parameter it is being told to send; hiding it would leave the validator's
    // error without a matching provenance entry.
    let schema = json!({
        "type": "object",
        "properties": { "a": { "type": "string", "default": "x" } },
        "required": ["a", "ghost"]
    });

    let origins = origins(&schema, &json!({}));

    assert_eq!(
        origins,
        vec![
            ("a".to_owned(), Provenance::Defaulted),
            ("ghost".to_owned(), Provenance::Absent),
        ]
    );
}

#[test]
fn provenance_ignores_non_object_arguments_and_non_object_schemas() {
    let schema = json!({ "type": "object", "properties": { "a": { "default": 1 } } });

    assert!(origins(&schema, &json!("not an object")).is_empty());
    assert!(origins(&json!(true), &json!({ "a": 1 })).is_empty());
    assert!(origins(&json!("not a schema"), &json!({})).is_empty());
}

#[test]
fn complete_from_defaults_leaves_non_object_arguments_alone() {
    let schema = json!({ "type": "object", "properties": { "a": { "default": 1 } } });
    let mut args = json!("not an object");

    let completion = complete_from_defaults(&schema, &mut args);

    assert_eq!(completion.arguments, json!("not an object"));
    assert!(completion.is_empty());
}

#[test]
fn a_deeply_nested_schema_terminates() {
    // A self-referencing shape is not expressible in JSON, but a deeply nested one is. The walk must
    // stop rather than overflow the stack on a generated schema.
    fn nest(depth: usize) -> (Value, Value) {
        let mut schema = json!({ "type": "object", "properties": {} });
        let mut args = json!({});
        for _ in 0..depth {
            schema = json!({ "type": "object", "properties": { "child": schema } });
            args = json!({ "child": args });
        }
        (schema, args)
    }
    let (schema, mut args) = nest(200);

    let completion = complete_from_defaults(&schema, &mut args);

    assert!(completion.is_empty());
}

#[test]
fn provenance_of_a_partially_completed_nested_call() {
    let schema = json!({
        "type": "object",
        "properties": {
            "path": { "type": "string" },
            "options": {
                "type": "object",
                "properties": {
                    "mode": { "type": "string", "default": "ro" },
                    "strict": { "type": "boolean" }
                }
            }
        },
        "required": ["path"]
    });
    let mut args = json!({ "path": "a.txt", "options": {} });
    let origins = origins(&schema, &args);

    let completion = complete_from_defaults(&schema, &mut args);

    assert_eq!(
        completion.filled,
        vec!["options.mode".to_owned()],
        "nested paths are rooted at the parameter, not at the call"
    );
    assert_eq!(
        origins,
        vec![
            ("options".to_owned(), Provenance::Provided),
            ("path".to_owned(), Provenance::Provided),
        ],
        "the top-level view classifies the parameter, not its children"
    );
}

#[test]
fn a_completed_call_still_validates_under_the_defaults_it_used() {
    // Completion is meant to run before validation, so the pair has to agree on the value it adds:
    // a filled default of the wrong JSON type would turn a missing argument into a type violation.
    let schema = json!({
        "type": "object",
        "properties": {
            "path": { "type": "string" },
            "limit": { "type": "integer", "default": 10 }
        },
        "required": ["path"]
    });
    let mut args = json!({ "path": "a.txt" });

    let completion = complete_from_defaults(&schema, &mut args);

    let filled = completion
        .arguments
        .get("limit")
        .and_then(Value::as_i64)
        .expect("the default arrives as the schema's type");
    assert_eq!(filled, 10);
}

#[test]
fn filled_paths_quote_keys_that_are_not_identifiers() {
    let schema = json!({
        "type": "object",
        "properties": { "x-mode": { "type": "string", "default": "ro" } }
    });

    assert_eq!(
        filled_paths(&schema, &json!({})),
        vec!["[\"x-mode\"]".to_owned()]
    );
    assert_eq!(Provenance::Absent.as_str(), "absent");
    assert_eq!(Provenance::Defaulted.as_str(), "defaulted");
    assert_eq!(Provenance::Provided.as_str(), "provided");
}
