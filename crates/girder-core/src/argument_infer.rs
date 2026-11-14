//! Filling a tool call's arguments from the defaults its schema declares.
//!
//! [`crate::tool_contracts`] answers "is this call acceptable?" and says no. It cannot say "this
//! call was acceptable except that `encoding` was omitted and the schema gives it a default". That
//! leaves a real gap: a model that reads "`args.encoding` is required but was not supplied" for a
//! parameter whose schema declares `"default": "utf8"` is being told to do work the schema already
//! did, and the only visible outcome is a wasted turn.
//!
//! This module closes that gap from the other side. It never decides whether a call is valid —
//! `required` stays [`crate::tool_contracts`]'s business — it only makes a call *more complete*
//! before it is validated. The two modules therefore compose in one direction: complete first,
//! validate second. Nothing here reads `required` to decide a violation, and nothing here may
//! report one.
//!
//! # Why only `default`, never a type's zero value
//!
//! It is tempting to fill `""` for a missing string or `0` for a missing number, because both
//! satisfy a `type` check and make validation pass. That is exactly the harm: validation passing is
//! not the same as the call doing what the model meant. A tool that receives `""` for a path the
//! model never mentioned will act on the empty path instead of reporting that the argument was
//! omitted, and the failure surfaces later, somewhere unrelated, with no record of the guess. A
//! schema author who wrote `"default"` made a statement about the intended value; nobody made that
//! statement about `0`.
//!
//! The same reasoning is why an *optional* parameter with no default is left missing rather than
//! filled with `null`. "Not provided" and "provided as null" are different facts, and several
//! tools branch on the difference (clear-a-field versus leave-it-alone). Materializing `null`
//! would turn a no-op into a request.
//!
//! # What the caller can tell apart afterwards
//!
//! [`Completion::filled`] lists the parameters this module supplied, and [`provenance`] classifies
//! every parameter as [`Provenance::Provided`], [`Provenance::Defaulted`] or [`Provenance::Absent`].
//! That classification is the module's most important output: it is what lets an error message say
//! "the model omitted `path`" instead of "`path` is missing", and what lets a prompt say "you left
//! out `encoding`, so `utf8` was used" without inventing a value the model supplied.

use serde_json::{Map, Value};

/// How deep the walk descends before it stops.
///
/// Matches [`crate::tool_contracts`]'s limit for the same reason: a hand-written or generated schema
/// can nest arbitrarily, and a recursive fill that follows it without bound turns a malformed
/// schema into a stack overflow, which is a process abort rather than an error. Stopping keeps the
/// failure reportable.
const MAX_DEPTH: usize = 64;

/// The result of one completion pass.
///
/// Returned by value, including a copy of the completed arguments, so a caller can log or forward
/// the result after it has already moved the mutated value onward.
#[derive(Clone, Debug)]
pub struct Completion {
    /// The arguments after completion. Equal to the `&mut` value the call was given.
    pub arguments: Value,
    /// The parameter paths that were supplied from a schema `default`, in fill order.
    ///
    /// Paths are dotted from the parameter itself — `encoding`, `options.mode` — not prefixed with
    /// the whole call, because these strings are meant to be pasted into a message to the model.
    pub filled: Vec<String>,
}

impl Completion {
    /// Whether the schema had nothing to add.
    ///
    /// A caller that wants to skip re-serializing an unchanged call, or that wants to offer a
    /// hint only when a default was actually used, asks this instead of inspecting `filled`.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.filled.is_empty()
    }
}

/// Where one parameter's value came from.
///
/// The three cases are kept apart on purpose. Collapsing [`Self::Defaulted`] into
/// [`Self::Provided`] hides that the model never said the value; collapsing [`Self::Absent`] into
/// [`Self::Defaulted`] hides that no default exists and the omission is the model's to fix. A
/// diagnostic that cannot separate them has to hedge, and a hedge ("`encoding` is missing or
/// defaulted") tells the reader less than either single statement.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Provenance {
    /// The model supplied it. An explicit `null` counts: it is a value the model chose.
    Provided,
    /// Missing from the call, supplied here from the schema's `default`.
    Defaulted,
    /// Missing from the call, and the schema declares no default for it.
    Absent,
}

impl Provenance {
    /// A stable lowercase name, for log lines and structured diagnostics.
    ///
    /// Spelled out rather than derived from `Debug` so that a log parser does not depend on the
    /// Rust type's formatting, which is free to change.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Provided => "provided",
            Self::Defaulted => "defaulted",
            Self::Absent => "absent",
        }
    }
}

/// Fills missing parameters from the `default` each property's schema declares.
///
/// Only properties the call omits **and** whose schema carries a `default` key are filled; a
/// `default` of `null` is a real default and is filled as such. A missing optional parameter
/// without a default stays missing, and so does a missing required one without a default — this
/// function is not the place that decides whether a call is valid, so it neither invents a value
/// for a required parameter nor reports the omission.
///
/// A parameter that is required *and* has a default is filled like any other. The schema author
/// wrote the default down; refusing to apply it would fail a call that the schema itself describes
/// as having a defined value, and would do so with an error the reader cannot act on. [`provenance`]
/// still records that the model did not send it.
///
/// Nested objects are handled symmetrically: if the call already contains an object for a property,
/// that object's own missing defaulted properties are filled and their paths reported as
/// `outer.inner`. An object property that the call omits entirely is **not** synthesized even when
/// its sub-schemas declare defaults — creating `{}`-shaped parents would be the "provided an empty
/// value" mistake one level up. Array elements (`items`) are deliberately not descended into: a
/// default materialized inside an element is a field the model did not send for a value it did,
/// and the element count the call chose is a signal this module should not rewrite.
pub fn complete_from_defaults(schema: &Value, arguments: &mut Value) -> Completion {
    let mut filled = Vec::new();
    fill(schema, arguments, "", 0, &mut filled);
    Completion {
        arguments: arguments.clone(),
        filled,
    }
}

/// Reports each declared parameter's origin, using the same rule [`complete_from_defaults`] applies
/// without mutating anything.
///
/// Call this with the arguments **as the model sent them**, before completion. After
/// [`complete_from_defaults`] has run, a defaulted parameter is indistinguishable from a provided
/// one — a model that sends exactly the default value looks identical to one that sent nothing — so
/// this function would report it as [`Provenance::Provided`]. A caller that has already completed
/// the call should ask [`Completion::filled`] instead; the two views are complementary, not
/// alternative, and neither can reconstruct the other.
///
/// Order is deterministic: properties in schema order (the map is a `BTreeMap`, so key order), then
/// any name listed in `required` that `properties` does not declare. The latter has no schema to
/// read a default from, so it can only be [`Provenance::Provided`] or [`Provenance::Absent`] —
/// reporting it at all matters because a `required` entry missing from `properties` is a real
/// authoring mistake that a caller would otherwise never see reflected in a per-parameter view.
///
/// Arguments that are not a JSON object have no parameters, so the result is empty: there is
/// nothing to classify, and the type mismatch is [`crate::tool_contracts`]'s to report.
#[must_use]
pub fn provenance(schema: &Value, arguments: &Value) -> Vec<(String, Provenance)> {
    let Some(node) = schema.as_object() else {
        return Vec::new();
    };
    let Some(object) = arguments.as_object() else {
        return Vec::new();
    };
    let properties = node.get("properties").and_then(Value::as_object);
    let mut origins = Vec::new();
    if let Some(properties) = properties {
        for (name, child) in properties {
            let present = object.contains_key(name);
            origins.push((name.clone(), classify(present, has_default(child))));
        }
    }
    for name in required_names(node) {
        let declared = properties.is_some_and(|properties| properties.contains_key(name.as_str()));
        if declared {
            continue;
        }
        let present = object.contains_key(name.as_str());
        origins.push((name.clone(), classify(present, false)));
    }
    origins
}

/// Classifies one parameter from the two facts that decide it.
///
/// A separate function because the two call sites above ask the same question with different
/// inputs, and a rule written twice is a rule that drifts.
const fn classify(present: bool, defaulted: bool) -> Provenance {
    if present {
        Provenance::Provided
    } else if defaulted {
        Provenance::Defaulted
    } else {
        Provenance::Absent
    }
}

/// Walks one object schema node, filling defaults and recursing into objects the call supplies.
fn fill(
    schema: &Value,
    arguments: &mut Value,
    prefix: &str,
    depth: usize,
    filled: &mut Vec<String>,
) {
    if depth > MAX_DEPTH {
        return;
    }
    let Some(node) = schema.as_object() else {
        return;
    };
    let Some(properties) = node.get("properties").and_then(Value::as_object) else {
        return;
    };
    let Some(object) = arguments.as_object_mut() else {
        return;
    };
    // Two passes over the same, small property map: the first only inserts, the second only
    // recurses. Interleaving them would mean recursing into a value inserted moments earlier, which
    // is harmless but reports a path the model never mentioned, so the split keeps `filled` honest.
    for (name, child) in properties {
        if object.contains_key(name) {
            continue;
        }
        if let Some(default) = child.get("default") {
            object.insert(name.clone(), default.clone());
            filled.push(join_path(prefix, name));
        }
    }
    for (name, child) in properties {
        let descends = child
            .as_object()
            .is_some_and(|child| child.contains_key("properties"));
        if !descends {
            continue;
        }
        let Some(field) = object.get_mut(name) else {
            continue;
        };
        fill(child, field, &join_path(prefix, name), depth + 1, filled);
    }
}

/// Whether a property schema declares a default at all.
///
/// Presence, not value: `"default": null` is a declaration that the intended value is `null`, and
/// reading it as "no default" would silently drop it.
fn has_default(schema: &Value) -> bool {
    schema
        .as_object()
        .is_some_and(|node| node.contains_key("default"))
}

/// The string names in a schema's `required`, in declaration order.
///
/// Non-strings are skipped rather than reported: a malformed `required` already produces a
/// violation in [`crate::tool_contracts`], and duplicating that diagnosis here would give the same
/// defect two different messages.
fn required_names(node: &Map<String, Value>) -> Vec<String> {
    node.get("required")
        .and_then(Value::as_array)
        .map(|names| {
            names
                .iter()
                .filter_map(Value::as_str)
                .map(str::to_owned)
                .collect()
        })
        .unwrap_or_default()
}

/// Appends one parameter name to a dotted path rooted at the call, not at `args`.
///
/// The bracket form for keys that are not identifiers mirrors the path spelling the validator uses,
/// so a path copied from a violation and a path reported here name the same thing.
fn join_path(prefix: &str, name: &str) -> String {
    let identifier = !name.is_empty() && name.chars().all(|ch| ch.is_alphanumeric() || ch == '_');
    match (prefix.is_empty(), identifier) {
        (true, true) => name.to_owned(),
        (true, false) => format!("[{name:?}]"),
        (false, true) => format!("{prefix}.{name}"),
        (false, false) => format!("{prefix}[{name:?}]"),
    }
}
