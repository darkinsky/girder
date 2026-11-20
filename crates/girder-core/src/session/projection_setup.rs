//! Assembling the projection registry a deployment serves.
//!
//! # The gap this closes
//!
//! `ProjectionRegistry` had ten implementations and **zero registrations in production code**: every
//! `impl Projection for …` existed, several had tests, and `serve` created an empty registry. So
//! `session.state` answered `projections: {}` and `session.observeProjections` streamed an empty map
//! -- not because anything failed, but because nothing was ever registered.
//!
//! That is the same defect this whole pass kept meeting, one layer up: a subsystem complete in its
//! parts and never assembled.
//!
//! # Why one function
//!
//! A projection registry is a set, and a set assembled in three places is three sets. One function
//! means `serve` and a test cannot disagree about what a session's state contains, which is exactly
//! the drift `docs/port-map.md` exists to catch.

use std::sync::Arc;

use crate::session::projection::{
    ModelSelectionProjection, ProjectionRegistry, TitleProjection, TurnBoundary,
};

/// Builds the registry a session's state is folded from.
///
/// The order is upstream's: the log-shaped folds first, then the session-level ones. Ordering matters
/// only for a reader inspecting `keys`, because each projection folds independently -- but a stable
/// order is what makes two runs answer the same list.
///
/// # Errors
///
/// [`crate::session::projection::ProjectionError`] on a duplicate key, which can only happen if two
/// projections claim one name. Reported rather than swallowed: a duplicate silently dropped would
/// make one of them dead without saying so.
pub fn assemble() -> Result<Arc<ProjectionRegistry>, crate::session::projection::ProjectionError> {
    let mut registry = ProjectionRegistry::new();
    for projection in base_projections() {
        registry.register(projection)?;
    }
    Ok(Arc::new(registry))
}

/// The projections this build can fold.
///
/// A `Vec<Arc<dyn Projection>>` rather than inlined registrations, so a caller that wants to add one
/// adds it here and every consumer sees it.
///
/// # Why these seven, and not every projection that exists
///
/// These are the folds whose upstream consumer is the Host itself: a session's state, its stats, its
/// turn outline, its token measurement, its working directory. A projection whose only reader is a
/// browser (`turnOutline`'s rail) still belongs here, because the Host serves it; one that needs a
/// subsystem this build lacks does not, and is named in `docs/final-report.md` instead.
#[must_use]
pub fn base_projections() -> Vec<Arc<dyn crate::session::projection::Projection>> {
    use crate::session::stats_rollup::Untimed;
    vec![
        // The reference fold: which turn is open, and where it started.
        Arc::new(TurnBoundary),
        // What model the session has used, so a picker opens on the one in effect.
        Arc::new(ModelSelectionProjection),
        // The session's title and what produced it.
        Arc::new(TitleProjection),
        // Whole-log turn and step counts with wall times. `Untimed` is the right source here: this
        // build's events carry their own timestamps, and a projection that dated them again would
        // disagree with the log it folded.
        Arc::new(crate::session::stats_rollup::StatsProjection::new(Arc::new(
            Untimed,
        ))),
        // Every started turn with the seq a jump can page through.
        Arc::new(crate::session::proj_extra::TurnOutlineProjection),
        // The model-list metadata a session carries, so a picker can render a row without reading
        // the whole log.
        Arc::new(crate::session::top::SessionListMetadataProjection::new(
            Arc::new(Untimed),
        )),
        // Whether a session is eligible to be continued, which is what `session.continuation` reads.
        Arc::new(crate::session::top::ContinuationProjection),
        // The token measurement fold: what a retry can reuse instead of re-measuring.
        Arc::new(crate::llm::token_measurement::TokenMeasurementProjection),
        // The working directory in effect, which a shell tool reads before running anything.
        Arc::new(crate::shell::working_dir::WorkingDirectoryProjection),
    ]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn assembling_registers_every_projection() {
        // The defect this module closes: ten implementations, zero registrations.
        let registry = assemble().expect("assembled");
        let keys = registry.keys();
        assert!(keys.contains(&"turnBoundary"), "{keys:?}");
        assert!(keys.contains(&"modelSelection"), "{keys:?}");
        assert!(keys.contains(&"title"), "{keys:?}");
        assert!(keys.contains(&"sessionStats"), "{keys:?}");
        assert!(keys.contains(&"turnOutline"), "{keys:?}");
    }

    #[test]
    fn the_registry_is_not_empty() {
        // The symptom that started this: `session.state` answered `projections: {}`.
        let registry = assemble().expect("assembled");
        assert!(!registry.keys().is_empty());
    }

    #[test]
    fn keys_are_stable_across_assemblies() {
        // Two runs of the same build must answer the same list.
        assert_eq!(
            assemble().expect("first").keys(),
            assemble().expect("second").keys()
        );
    }
}
