//! Tests for the agent (M4 of the v2 plan).
//!
//! The model-selection precedence is the part with real semantics: three sources disagree, and
//! getting the order wrong silently undoes a caller's explicit choice.

use girder_core::{
    Agent, AgentOptions, AgentSpec, ChatMessage, InboxTarget, MessageId, ModelInfo, ModelSelection,
    ModelSelectionState, Route, RouteRegistry, SelectionError, SelectionSource, SessionId, Splice,
};

fn session() -> SessionId {
    SessionId::new("session-1").expect("valid")
}

fn options() -> AgentOptions {
    AgentOptions::new("ds", "deepseek-chat")
}

// ---- completeness -----------------------------------------------------------------------------

#[test]
fn options_naming_both_halves_produce_a_selection() {
    let selection = options().selection().expect("complete");

    assert_eq!(selection.provider, "ds");
    assert_eq!(selection.model, "deepseek-chat");
}

#[test]
fn options_naming_only_a_provider_produce_nothing() {
    // A partial pair is not a usable default: neither half resolves alone, and treating them
    // separately would pair a provider with a model another source named.
    let partial = AgentOptions {
        provider: Some("ds".to_owned()),
        ..AgentOptions::default()
    };

    assert!(partial.selection().is_none());
}

#[test]
fn options_naming_only_a_model_produce_nothing() {
    let partial = AgentOptions {
        model: Some("m".to_owned()),
        ..AgentOptions::default()
    };

    assert!(partial.selection().is_none());
}

// ---- precedence -------------------------------------------------------------------------------

#[test]
fn the_live_options_apply_when_nothing_else_does() {
    let state = ModelSelectionState::new(options());
    let (selection, source) = state.current();

    assert_eq!(selection.map(|s| s.model), Some("deepseek-chat".to_owned()));
    assert_eq!(source, Some(SelectionSource::Options));
}

#[test]
fn the_last_used_selection_beats_the_options() {
    // Evidence of what worked outranks a default.
    let mut state = ModelSelectionState::new(options());
    state.note_used(ModelSelection::new("ds", "deepseek-reasoner"));

    let (selection, source) = state.current();

    assert_eq!(
        selection.map(|s| s.model),
        Some("deepseek-reasoner".to_owned())
    );
    assert_eq!(source, Some(SelectionSource::LastUsed));
}

#[test]
fn a_recorded_selection_beats_everything() {
    // An explicit instruction must survive a restart and must not be undone by a later default.
    let mut state = ModelSelectionState::new(options());
    state.note_used(ModelSelection::new("ds", "deepseek-reasoner"));
    state.record(ModelSelection::new("other", "m2")).unwrap();

    let (selection, source) = state.current();

    assert_eq!(selection.map(|s| s.provider), Some("other".to_owned()));
    assert_eq!(source, Some(SelectionSource::Recorded));
}

#[test]
fn clearing_the_recorded_selection_falls_back_to_last_used() {
    let mut state = ModelSelectionState::new(options());
    state.note_used(ModelSelection::new("ds", "deepseek-reasoner"));
    state.record(ModelSelection::new("other", "m2")).unwrap();

    assert!(state.clear_recorded());
    let (selection, source) = state.current();

    assert_eq!(
        selection.map(|s| s.model),
        Some("deepseek-reasoner".to_owned())
    );
    assert_eq!(source, Some(SelectionSource::LastUsed));
}

#[test]
fn clearing_twice_reports_nothing_cleared() {
    // So a caller can avoid a no-op log entry.
    let mut state = ModelSelectionState::new(options());
    state.record(ModelSelection::new("a", "b")).unwrap();

    assert!(state.clear_recorded());
    assert!(!state.clear_recorded());
}

#[test]
fn recording_an_equal_selection_is_a_no_op() {
    // A UI that re-sends its current choice must not write a log entry per click.
    let mut state = ModelSelectionState::new(options());

    assert!(state.record(ModelSelection::new("a", "b")).unwrap());
    assert!(!state.record(ModelSelection::new("a", "b")).unwrap());
}

#[test]
fn a_later_recorded_selection_replaces_the_earlier_one() {
    let mut state = ModelSelectionState::new(options());
    state.record(ModelSelection::new("a", "b")).unwrap();
    state.record(ModelSelection::new("c", "d")).unwrap();

    assert_eq!(state.next().map(|s| s.model), Some("d".to_owned()));
}

#[test]
fn nothing_resolves_when_no_source_names_both_halves() {
    let state = ModelSelectionState::new(AgentOptions::default());
    let (selection, source) = state.current();

    assert!(selection.is_none());
    assert!(source.is_none());
}

// ---- the view ---------------------------------------------------------------------------------

#[test]
fn the_view_separates_next_from_last_used() {
    // A UI shows both: a switch is invisible until the step runs if they are collapsed, and a failed
    // switch looks like it took effect.
    let mut state = ModelSelectionState::new(options());
    state.note_used(ModelSelection::new("ds", "deepseek-chat"));
    state
        .record(ModelSelection::new("ds", "deepseek-reasoner"))
        .unwrap();

    let view = state.view();

    assert_eq!(
        view.next.map(|s| s.model),
        Some("deepseek-reasoner".to_owned())
    );
    assert_eq!(
        view.last_used.map(|s| s.model),
        Some("deepseek-chat".to_owned())
    );
}

#[test]
fn the_view_reports_next_while_options_supply_it() {
    let state = ModelSelectionState::new(options());
    let view = state.view();

    assert!(view.next.is_some());
    assert!(view.last_used.is_none(), "nothing has been routed yet");
}

// ---- validation -------------------------------------------------------------------------------

#[test]
fn a_selection_for_a_mounted_route_validates() {
    let mut registry = RouteRegistry::new();
    registry
        .register(Route::new("ds", "deepseek").with_model(ModelInfo::text("m", 100, 10)))
        .unwrap();

    girder_core::validate_model_selection(&ModelSelection::new("ds", "m"), &registry)
        .expect("valid");
}

#[test]
fn an_unknown_provider_is_refused() {
    let registry = RouteRegistry::new();
    let error = girder_core::validate_model_selection(&ModelSelection::new("nope", "m"), &registry)
        .expect_err("unknown provider");

    assert!(matches!(error, SelectionError::UnknownProvider { .. }));
}

#[test]
fn an_unknown_model_is_refused() {
    let mut registry = RouteRegistry::new();
    registry
        .register(Route::new("ds", "deepseek").with_model(ModelInfo::text("m", 100, 10)))
        .unwrap();

    let error =
        girder_core::validate_model_selection(&ModelSelection::new("ds", "other"), &registry)
            .expect_err("unknown model");

    let SelectionError::UnknownModel { provider, model } = error else {
        panic!("expected UnknownModel");
    };
    assert_eq!((provider.as_str(), model.as_str()), ("ds", "other"));
}

// ---- the agent --------------------------------------------------------------------------------

#[test]
fn an_agent_carries_its_identity() {
    let agent = Agent::new(AgentSpec::root(session(), options()));

    assert_eq!(agent.session_id(), &session());
    assert!(agent.cwd().is_none());
}

#[test]
fn an_agent_reports_its_live_options() {
    let agent = Agent::new(AgentSpec::root(session(), options()));

    assert_eq!(
        agent.next_model().map(|s| s.model),
        Some("deepseek-chat".to_owned())
    );
}

#[test]
fn an_agent_records_a_switch() {
    let agent = Agent::new(AgentSpec::root(session(), options()));

    assert!(
        agent
            .select_model(ModelSelection::new("ds", "deepseek-reasoner"))
            .unwrap()
    );

    assert_eq!(
        agent.next_model().map(|s| s.model),
        Some("deepseek-reasoner".to_owned())
    );
}

#[test]
fn an_agent_accepts_inbox_splices() {
    let agent = Agent::new(AgentSpec::root(session(), options()));
    let message = ChatMessage::user(MessageId::new("m1").unwrap(), "hello");

    agent
        .splice_inbox(&Splice::append(InboxTarget::NextStep, vec![message]))
        .expect("spliced");

    assert_eq!(agent.inbox_state().next_step().len(), 1);
}

#[test]
fn cancelling_discards_the_inbox_by_default() {
    let mut agent = Agent::new(AgentSpec::root(session(), options()));
    let message = ChatMessage::user(MessageId::new("m1").unwrap(), "hello");
    agent
        .splice_inbox(&Splice::append(InboxTarget::NextStep, vec![message]))
        .unwrap();

    agent.cancel(false);

    assert!(agent.inbox_state().is_empty());
    agent.reset_cancel();
    assert!(
        !agent.cancel_token().is_cancelled(),
        "a fresh turn needs a fresh token"
    );
}

#[test]
fn cancelling_can_keep_the_inbox() {
    // A user interrupt usually wants to keep what they typed, while shutting down does not.
    let agent = Agent::new(AgentSpec::root(session(), options()));
    let message = ChatMessage::user(MessageId::new("m1").unwrap(), "hello");
    agent
        .splice_inbox(&Splice::append(InboxTarget::NextStep, vec![message]))
        .unwrap();

    agent.cancel(true);

    assert_eq!(agent.inbox_state().next_step().len(), 1);
    assert!(agent.cancel_token().is_cancelled());
}

#[test]
fn the_cancel_token_cannot_be_uncancelled() {
    // A `CancellationToken` has no reset, which is why `reset_cancel` exists and is explicit: a
    // caller that forgets starts every turn already cancelled.
    let mut agent = Agent::new(AgentSpec::root(session(), options()));
    let before = agent.cancel_token();
    agent.cancel(false);
    assert!(before.is_cancelled());

    agent.reset_cancel();

    assert!(!agent.cancel_token().is_cancelled());
    assert!(before.is_cancelled(), "the old token stays cancelled");
}

#[test]
fn a_subagent_spec_carries_its_depth() {
    let spec = AgentSpec::root(session(), options())
        .with_depth(girder_core::Depth::root().child())
        .with_cwd("/work");

    assert_eq!(spec.depth.get(), 1);
    assert_eq!(spec.cwd.as_deref(), Some(std::path::Path::new("/work")));
}
