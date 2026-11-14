//! Falling back to another model when the selected one cannot serve the request.
//!
//! [`::girder_core::model_catalog_extra`] answers "which models can do X" for a whole catalog. A
//! request asks the mirror-image question: it arrives with a shape — it carries tool definitions, an
//! image, a reasoning effort, a history of a certain size — and one model was already chosen for it.
//! When that model cannot serve the shape, the caller has three options: fail the request, drop the
//! requirement, or serve the request from a different model. This module implements the third and,
//! when no model fits, hands back enough information for the caller to make the second one.
//!
//! # Why preference orders but never filters
//!
//! A preference score says which model a deployment would *like* to serve, so it belongs in the
//! ordering of the models that can serve the request. It must not admit one that cannot: a high score
//! is not a capability, and serving a request that needs tools from a fast model without them turns
//! a working request into a provider-side error. [`choose`] therefore filters by requirement first
//! and orders by preference second.
//!
//! # Why the failure counts one requirement at a time
//!
//! "No model fits" is not actionable. A deployment with no vision model needs a different fix from
//! one whose models all have too small a window, and the caller cannot tell them apart from a bare
//! `None`. [`Shortfall`] records, for every *stated* requirement, how many candidates would become
//! usable if that requirement alone were relaxed. Counting each requirement in isolation is what
//! turns the numbers into advice: a candidate that fails tools *and* vision is admitted by relaxing
//! neither one alone, so it is counted under neither, and a caller reading "tools: 5, vision: 1"
//! learns that relaxing tools is the useful move.
//!
//! # Why an unknown constraint is not a failure
//!
//! A model that does not state its context window may still be large enough. Treating "unknown" as
//! "too small" would exclude it on the strength of a fact nobody claimed, which is the same reasoning
//! [`::girder_core::model_catalog_extra`] applies to its filter. The excluded-by-unknown alternative also has
//! no better authority behind it: the provider is the only party that can answer, and it answers when
//! the request is sent.
//!
//! # Why the choice does not depend on input order
//!
//! Two callers holding the same catalog must pick the same model, or a request's routing depends on
//! how the catalog happened to be assembled. Preference decides first; a tie is broken by
//! `(provider, model)`, which identifies a route and is stable across runs.

use std::collections::BTreeMap;
use std::fmt;

// The crate's own items are named through `::girder_core` rather than through `crate::` so that this
// file can be compiled both as a module of the library and, by `tests/model_fallback.rs`, as a module
// of the test crate before `lib.rs` re-exports it. `extern crate self as girder_core` in `lib.rs` is
// what makes that path resolve from inside the crate.
use ::girder_core::ModelCapabilities as Capabilities;

/// What one request needs from the model that serves it.
///
/// Each flag states a requirement when it is `true`; `false` means the request does not care, exactly
/// as [`Capabilities`] reads as a requirement elsewhere. `min_context` follows the same rule: `None`
/// asks for nothing, and a candidate whose window is unknown satisfies any `Some` requirement.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Needs {
    /// The request carries tool definitions the model must accept.
    pub tools: bool,
    /// The request carries image input the model must accept.
    pub vision: bool,
    /// The request sets a reasoning effort the model must accept.
    pub reasoning: bool,
    /// The smallest context window that can hold the request, in tokens.
    pub min_context: Option<u64>,
}

/// One model that could serve a request.
///
/// `provider` and `model` are the addressable route, and the pair is unique enough to break
/// preference ties. The two window fields exist side by side because a deployment may state a
/// window that differs from the model's published one — a proxy or a plan limit — and the
/// route-level value is the one that applies to this candidate.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Candidate {
    /// The provider route that serves the model.
    pub provider: String,
    /// The model id a request would name.
    pub model: String,
    /// What the model can do.
    pub capabilities: Capabilities,
    /// The window this route is held to, when the deployment states one apart from the model's.
    ///
    /// Wins over `capabilities.context_window`; when neither is stated the candidate is not excluded
    /// from a size requirement, because an unknown window is unverified rather than insufficient.
    pub context_window: Option<u64>,
    /// How much the deployment wants to serve this candidate; higher wins.
    pub preference: u32,
}

/// Why no candidate could serve a request.
///
/// Both fields describe the same failure: `eliminated` is the machine-readable form, and `message` is
/// one sentence naming the requirement whose relaxation would help most.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Shortfall {
    /// Per stated requirement, how many candidates would be usable if that requirement alone were
    /// relaxed.
    ///
    /// A candidate that fails several requirements counts under none of them, because relaxing one
    /// of the failed requirements would not admit it. Keys are the requirement names a caller sees in
    /// [`Shortfall::message`], and only requirements the candidate set was actually asked about are
    /// present, so a missing key means "not required", never "no effect".
    pub eliminated: BTreeMap<String, usize>,
    /// One sentence explaining the shortfall, naming the requirement worth relaxing first.
    pub message: String,
}

impl Shortfall {
    /// Builds the description of a request no candidate serves.
    fn describe(needs: &Needs, candidates: &[Candidate]) -> Self {
        let mut eliminated: BTreeMap<String, usize> = BTreeMap::new();
        for requirement in Requirement::ALL {
            if !requirement.stated(needs) {
                continue;
            }
            // Count candidates this requirement alone keeps out. Requiring every other stated
            // requirement to pass is what makes the number mean "relaxing this one would admit
            // this many", instead of "this many candidates happen to miss it".
            let count = candidates
                .iter()
                .filter(|candidate| {
                    !requirement.met(needs, candidate)
                        && Requirement::ALL
                            .iter()
                            .all(|other| *other == requirement || other.met(needs, candidate))
                })
                .count();
            eliminated.insert(requirement.name().to_owned(), count);
        }

        let message = summarize(candidates.len(), &eliminated);
        Self {
            eliminated,
            message,
        }
    }
}

impl fmt::Display for Shortfall {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.message)
    }
}

impl std::error::Error for Shortfall {}

/// Picks the highest-preference candidate that satisfies every stated requirement.
///
/// Requirements are a hard precondition: a candidate that cannot serve the request is skipped no
/// matter how high its preference. Among the models that can serve it, the highest preference wins,
/// and a tie is resolved by `(provider, model)` so the choice is independent of catalog order.
///
/// # Errors
///
/// Returns a [`Shortfall`] when no candidate satisfies every requirement, including when `candidates`
/// is empty. The shortfall names, per requirement, how many candidates relaxing it alone would admit,
/// so the caller can report the useful escape hatch rather than a bare "no model fits".
pub fn choose<'a>(needs: &Needs, candidates: &'a [Candidate]) -> Result<&'a Candidate, Shortfall> {
    let mut best: Option<&Candidate> = None;
    for candidate in candidates {
        if !satisfies(needs, candidate) {
            continue;
        }
        let take = match best {
            Some(current) => outranks(candidate, current),
            None => true,
        };
        if take {
            best = Some(candidate);
        }
    }
    best.ok_or_else(|| Shortfall::describe(needs, candidates))
}

/// Whether `candidate` satisfies every stated requirement.
fn satisfies(needs: &Needs, candidate: &Candidate) -> bool {
    Requirement::ALL
        .iter()
        .all(|requirement| requirement.met(needs, candidate))
}

/// Whether `candidate` should be chosen over `current`.
///
/// Higher preference wins; on a tie the smaller `(provider, model)` wins, which makes the result a
/// function of the candidate set and not of its order.
fn outranks(candidate: &Candidate, current: &Candidate) -> bool {
    candidate
        .preference
        .cmp(&current.preference)
        // Reversed operands: the sort is ascending on the route pair, so the smaller route is the
        // preferred one.
        .then_with(|| current.provider.cmp(&candidate.provider))
        .then_with(|| current.model.cmp(&candidate.model))
        .is_gt()
}

/// The window a candidate is held to, if anything states one.
///
/// The route-level field wins because it is the deployment's own statement about this route; the
/// model's published window is the fallback.
fn window(candidate: &Candidate) -> Option<u64> {
    candidate
        .context_window
        .or(candidate.capabilities.context_window)
}

/// Whether a candidate's window can hold a request that needs `required` tokens.
///
/// Unknown is treated as unverifiable rather than insufficient, mirroring the catalog filter: refusing
/// a model for a fact nobody claimed would hide a usable model from every caller.
fn context_suffices(candidate: &Candidate, required: Option<u64>) -> bool {
    match (window(candidate), required) {
        // Nothing asked, or nothing stated: the requirement cannot exclude the candidate.
        (_, None) | (None, Some(_)) => true,
        (Some(declared), Some(required)) => declared >= required,
    }
}

/// One thing a request can need, so that "does it fit" and "what would relaxing it admit" are
/// answered by the same code and cannot drift apart.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Requirement {
    /// Tool calling.
    Tools,
    /// Image input.
    Vision,
    /// A reasoning effort setting.
    Reasoning,
    /// A context window of at least a stated size.
    MinContext,
}

impl Requirement {
    /// Every requirement, in the order this module reports them.
    const ALL: [Self; 4] = [Self::Tools, Self::Vision, Self::Reasoning, Self::MinContext];

    /// The name this requirement appears under in [`Shortfall::eliminated`].
    const fn name(self) -> &'static str {
        match self {
            Self::Tools => "tools",
            Self::Vision => "vision",
            Self::Reasoning => "reasoning",
            Self::MinContext => "min_context",
        }
    }

    /// Whether the request states this requirement at all.
    const fn stated(self, needs: &Needs) -> bool {
        match self {
            Self::Tools => needs.tools,
            Self::Vision => needs.vision,
            Self::Reasoning => needs.reasoning,
            Self::MinContext => needs.min_context.is_some(),
        }
    }

    /// Whether `candidate` meets this requirement; an unstated requirement is met by anything.
    fn met(self, needs: &Needs, candidate: &Candidate) -> bool {
        let capabilities = &candidate.capabilities;
        match self {
            Self::Tools => !needs.tools || capabilities.tools,
            Self::Vision => !needs.vision || capabilities.vision,
            Self::Reasoning => !needs.reasoning || capabilities.reasoning,
            Self::MinContext => context_suffices(candidate, needs.min_context),
        }
    }
}

/// One sentence describing the shortfall, naming the requirement worth relaxing.
///
/// The dominant requirement is the one with the largest count; a tie keeps the alphabetically first
/// key, so the same candidate set always produces the same sentence. When every count is zero, no
/// single relaxation would help, and the sentence says that instead of naming an arbitrary one.
fn summarize(total: usize, eliminated: &BTreeMap<String, usize>) -> String {
    if total == 0 {
        return "no candidates were offered, so there was nothing to satisfy the requirements"
            .to_owned();
    }

    let mut dominant: Option<(&str, usize)> = None;
    for (name, count) in eliminated {
        let better = match dominant {
            Some((_, best)) => *count > best,
            None => *count > 0,
        };
        if better {
            dominant = Some((name.as_str(), *count));
        }
    }

    match dominant {
        Some((name, count)) => format!(
            "no candidate satisfies every stated requirement; relaxing `{name}` alone would admit \
             the most, {count} of {total} candidates"
        ),
        None => format!(
            "no candidate satisfies every stated requirement, and each was eliminated by two or \
             more of them, so relaxing one alone would admit none of the {total} candidates"
        ),
    }
}

/// Builds the candidates a route table offers, so a caller can ask [`choose`] about real models rather than hand-built
/// ones.
///
/// Preference is the model's position within its route, later models scoring higher, which is only a starting point: a
/// deployment with a real opinion sets `preference` itself, and this exists so the common case (one route, several
/// models) does not require a caller to invent a number.
#[must_use]
pub fn candidates_from_routes(routes: &girder_core::RouteRegistry) -> Vec<Candidate> {
    let mut out = Vec::new();
    for name in routes.names() {
        let Ok(route) = routes.get(&name) else {
            continue;
        };
        for (index, model) in route.models.iter().enumerate() {
            out.push(Candidate {
                provider: route.name.clone(),
                model: model.id.clone(),
                capabilities: girder_core::ModelCapabilities {
                    tools: model.supports_tools,
                    vision: model
                        .input_modalities
                        .iter()
                        .any(|kind| matches!(kind, girder_core::Modality::Image)),
                    reasoning: model.supports_reasoning,
                    context_window: model.context_window,
                    max_output: model.max_tokens,
                },
                context_window: model.context_window,
                // A `u32` by construction: a route holds a handful of models, and a cast that saturated at four billion
                // would be a different number than this loop can reach.
                preference: u32::try_from(index).unwrap_or(u32::MAX),
            });
        }
    }
    out
}
