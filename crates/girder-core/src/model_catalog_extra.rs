//! Model capabilities: what a model can do, how to ask a catalog for the models that can do it, and
//! how that catalog reads on the wire.
//!
//! [`crate::providers::ModelInfo`] already carries the facts — tool support, image input, reasoning,
//! and two token ceilings — but `session.modelCatalog` publishes only `provider`, `model`,
//! `displayName`, and `contextWindow`. A client that renders a model picker therefore knows how large
//! a model is and nothing about whether it can be given a tool schema or an image, so the only way to
//! find out is to send a request and read the failure. This module carries the missing half: a
//! declaration, a filter over it, and a renderer that adds it to the catalog without moving anything a
//! current client already reads.
//!
//! # Why a separate type rather than a flag per model
//!
//! [`Capabilities`] is the unit both halves of the problem are stated in. A caller that needs "a model
//! that takes images and reasons" expresses exactly that, once, and the same value drives
//! [`filter_by`]. Spreading the same five fields across two structures would let the filter and the
//! renderer disagree about which ones exist.
//!
//! # Why filtering treats `false` as "no opinion"
//!
//! This is the shape's one subtle contract, and the one worth stating twice. A [`Capabilities`] value
//! passed as a *requirement* answers "what must this model be able to do", not "what must this model
//! be unable to do". `tools: false` means the caller does not care about tool support, so a model that
//! does call tools still matches; it does not mean "a model that cannot call tools". Reading it the
//! other way — as an exact specification — would make every call site spell out all five fields just
//! to ask for one, and would silently turn `Capabilities::default()` into "match only models with no
//! abilities at all", which is the opposite of the useful answer to "no requirements".
//!
//! # Why `None` sizes are not failures
//!
//! A context window is `None` when nobody stated one, not when it is zero. Filtering a model out for
//! an unknown window would hide a usable model from the picker on the strength of a fact that was
//! never claimed — the same reasoning `ModelInfo::with_unknown_window` gives for compaction. So an
//! unknown size satisfies any requirement: it is unchecked, not rejected.
//!
//! # Why the ordering is `(provider, name)` and the sort is stable
//!
//! Two clients that both ask for "every model that takes images" must render the same list, or the
//! catalog is not a catalog. Sorting by the pair that identifies a model is deterministic without
//! depending on insertion order, and a stable sort keeps two entries that share a pair (the same
//! model mounted twice by different credentials) in the order the caller supplied, so a deliberate
//! ordering is not lost to the sort itself.

use serde_json::{Value, json};

/// What one model can do.
///
/// Every field is a fact the deployment states about a model, except the two sizes, which are
/// declared as facts *or* as their absence: `None` means "unknown", never "zero". The value doubles as
/// a requirement for [`filter_by`], where a `false` flag means "not required" — see the module docs.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Capabilities {
    /// Whether the model can be given tool definitions.
    pub tools: bool,
    /// Whether the model accepts image input.
    pub vision: bool,
    /// Whether the model accepts a reasoning effort setting and emits reasoning content.
    pub reasoning: bool,
    /// The largest context the model accepts, in tokens; `None` when unknown.
    pub context_window: Option<u64>,
    /// The largest output the model produces, in tokens; `None` when unknown.
    pub max_output: Option<u64>,
}

impl Capabilities {
    /// Whether this model satisfies every stated requirement in `need`.
    ///
    /// A requirement is stated only by a `true` flag or a `Some` size, which is what makes
    /// [`Capabilities::default`] mean "no requirements" rather than "no abilities".
    fn satisfies(&self, need: &Capabilities) -> bool {
        // `need.tools` is the requirement and `self.tools` the fact, in that order: reading this as
        // `self.tools || !need.tools` says "a model without the ability is acceptable only when the
        // caller did not ask for it", which is the whole semantics above.
        (!need.tools || self.tools)
            && (!need.vision || self.vision)
            && (!need.reasoning || self.reasoning)
            && at_least(self.context_window, need.context_window)
            && at_least(self.max_output, need.max_output)
    }
}

/// Whether a declared size satisfies a required one.
///
/// Unknown (`None`) is treated as unverifiable rather than insufficient. Rejecting it would refuse a
/// model because of a fact nobody claimed; accepting it leaves the request to be refused by the
/// provider, which is the only party that can actually answer.
fn at_least(declared: Option<u64>, required: Option<u64>) -> bool {
    match (declared, required) {
        // Nothing asked, or nothing stated: the requirement cannot exclude the model.
        (_, None) | (None, Some(_)) => true,
        (Some(declared), Some(required)) => declared >= required,
    }
}

/// One model a deployment serves, with what it can do.
///
/// `provider` and `name` are the pair that addresses the model — the route name and the model id — and
/// are what the catalog is ordered and grouped by. They are not display strings: a human-readable
/// label is the UI's business, and inventing one here would have to be re-localized per client.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ModelEntry {
    /// The provider route the model is served by.
    pub provider: String,
    /// The model id a request names.
    pub name: String,
    /// What the model can do.
    pub capabilities: Capabilities,
    /// Whether a UI may pre-select this model.
    ///
    /// A hint for a picker's initial choice, not a filter: a non-recommended model is still offered.
    /// Several models may be recommended; [`to_catalog_json`] picks the first one in catalog order as
    /// the default, which is why the field cannot claim to name *the* default.
    pub recommended: bool,
}

/// Returns the entries that satisfy every requirement in `need`, in catalog order.
///
/// A `true` flag or a `Some` size in `need` is a requirement; a `false` flag or `None` is no
/// requirement at all (see the module docs). An unknown size on an entry is not a failure: it is
/// included whenever the caller asked for a size, because unknown is not small.
///
/// The result borrows from `catalog`, so filtering does not copy strings or the catalog itself, and
/// it is ordered by `(provider, name)` — the order [`to_catalog_json`] renders — with ties left in
/// the caller's order.
#[must_use]
pub fn filter_by<'a>(catalog: &'a [ModelEntry], need: &Capabilities) -> Vec<&'a ModelEntry> {
    let mut matched: Vec<&ModelEntry> = catalog
        .iter()
        .filter(|entry| entry.capabilities.satisfies(need))
        .collect();
    // A stable sort: two mounts of one model keep the caller's order, which is the only order they
    // can be told apart by.
    matched.sort_by(|left, right| order(left, right));
    matched
}

/// The catalog order: provider, then model name.
fn order(left: &ModelEntry, right: &ModelEntry) -> std::cmp::Ordering {
    left.provider
        .cmp(&right.provider)
        .then_with(|| left.name.cmp(&right.name))
}

/// Renders a catalog as the `session.modelCatalog` value.
///
/// The outer shape is the one `register_model_catalog` already serves — `default`,
/// `routableProviders`, `groups`, `failures` — so a client written against it keeps working. What is
/// added is per-model: `capabilities` (grouped, so a future ability is one nested key rather than a
/// new top-level field that could collide), `maxOutput`, and `recommended`. `contextWindow` stays
/// where it was, which is why it is not repeated inside `capabilities`.
///
/// Unknown sizes render as `null` rather than as zero, matching the wire type's optional window: a
/// client that showed "0 tokens" would be claiming a fact nobody stated. A model id also serves as
/// its `displayName`, because [`ModelEntry`] carries no separate label.
///
/// `default` is the first recommended entry, or the first entry when nothing is recommended, or
/// `null` for an empty catalog. `failures` is always empty: a catalog that was handed a slice cannot
/// have failed to load a provider, and the field exists (as in the existing method) so a client's
/// warning list has a place to be empty rather than absent.
#[must_use]
pub fn to_catalog_json(catalog: &[ModelEntry]) -> Value {
    let ordered = filter_by(catalog, &Capabilities::default());

    // Group consecutive entries, which works because `filter_by` already sorted by provider: a
    // second grouping pass would need a map and would lose the order the sort just established.
    let mut groups: Vec<(&str, Vec<&ModelEntry>)> = Vec::new();
    for entry in &ordered {
        match groups.last_mut() {
            Some((provider, models)) if *provider == entry.provider => models.push(entry),
            _ => groups.push((entry.provider.as_str(), vec![*entry])),
        }
    }

    let rendered_groups: Vec<Value> = groups
        .iter()
        .map(|(provider, models)| {
            let models: Vec<Value> = models.iter().map(|entry| model_json(entry)).collect();
            json!({
                "provider": provider,
                "displayName": provider,
                "models": models,
            })
        })
        .collect();

    let default = ordered
        .iter()
        .find(|entry| entry.recommended)
        .or_else(|| ordered.first())
        .map_or(
            Value::Null,
            |entry| json!({ "provider": entry.provider, "model": entry.name }),
        );

    let routable: Vec<&str> = groups.iter().map(|(provider, _)| *provider).collect();

    json!({
        "default": default,
        "routableProviders": routable,
        "groups": rendered_groups,
        "failures": Vec::<Value>::new(),
    })
}

/// Renders one model as it appears inside a provider group.
///
/// Split out of [`to_catalog_json`] so the model shape has one definition: the group loop and any
/// future single-model renderer then cannot disagree about which keys a model carries.
fn model_json(entry: &ModelEntry) -> Value {
    json!({
        "provider": entry.provider,
        "model": entry.name,
        "displayName": entry.name,
        "contextWindow": entry.capabilities.context_window,
        "maxOutput": entry.capabilities.max_output,
        "capabilities": {
            "tools": entry.capabilities.tools,
            "vision": entry.capabilities.vision,
            "reasoning": entry.capabilities.reasoning,
        },
        "recommended": entry.recommended,
    })
}
