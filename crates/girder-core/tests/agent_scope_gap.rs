//! Tests for the agent-scope relation and the consumed-work fold.
//!
//! Two properties carry this module, and each is tested from the side that can fail.
//!
//! The first is that a scope's ancestry is *declared*, not inferred. The numeric encoding already
//! in `events.rs` derives a parent from the key's own bits, so a test that only walked a
//! well-formed chain would pass against either implementation and prove nothing. The tests here
//! therefore exercise the two cases the encoding cannot express: a key that keeps its identity
//! while adopting a new parent, and a key whose walk must stop at a recorded link rather than at a
//! numeric threshold.
//!
//! The second is that an accounting fold distinguishes work that ran from work that was lost. A
//! `turn/end{completed}` closes two very different logs — one that never claimed anything and one
//! whose claim was rewritten away — so the tests assert the *specific* field that separates them,
//! and assert the negative case (a cancellation that leaves work pending) as carefully as the
//! positive one.
//!
//! # Why the module is compiled by path
//!
//! `agent_scope_gap` is not exported from the crate root yet; the wiring is a separate change to
//! `lib.rs`. Compiling the exact source file here keeps these tests runnable and independent of
//! that wiring, while still exercising the same code a consumer will reach through the crate root.
//! Nothing is duplicated: this is the real file.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    reason = "tests assert by panicking; a failed unwrap is a failed test"
)]

#[path = "../src/agent_scope_gap.rs"]
mod agent_scope_gap;

use agent_scope_gap::{
    ConsumedWork, Event, ScopeError, ScopeGraph, SpliceOutcome, accounts_for_claim,
    fold_consumed_work,
};
use girder_core::TurnEndReason;

// ---- the scope relation ----------------------------------------------------------------------

#[test]
fn a_bound_chain_walks_nearest_first_and_stops_at_the_root() {
    let mut graph = ScopeGraph::new();
    let root = 10;
    let middle = 20;
    let leaf = 30;

    graph.bind(middle, root).unwrap();
    graph.bind(leaf, middle).unwrap();

    // Nearest first: the order a layer merge reverses so the closest scope wins.
    assert_eq!(graph.chain_of(Some(leaf)), vec![leaf, middle, root]);
    assert_eq!(graph.chain_of(Some(root)), vec![root]);
    assert_eq!(graph.parent_of(leaf), Some(middle));
    assert_eq!(graph.parent_of(root), None);
    assert_eq!(graph.len(), 2);
}

#[test]
fn an_unbound_key_and_an_absent_key_both_read_as_no_parent() {
    let mut graph = ScopeGraph::new();
    graph.bind(2, 1).unwrap();

    // A root and a key nobody ever minted answer the same way: distinguishing them would leak
    // whether a scope exists, and no caller needs that.
    assert_eq!(graph.parent_of(1), None);
    assert_eq!(graph.parent_of(999), None);
    // Reading is not a write, and an empty chain is the answer for "no key at all".
    assert!(graph.chain_of(None).is_empty());
    assert_eq!(graph.len(), 1);
}

#[test]
fn a_second_bind_from_outside_is_refused_and_names_the_key() {
    let mut graph = ScopeGraph::new();
    graph.bind(2, 1).unwrap();

    // Re-linking has exactly one owner. An outside bind must not quietly move a live subtree.
    assert_eq!(graph.bind(2, 3), Err(ScopeError::AlreadyBound { key: 2 }));
    // The refusal left the original link intact rather than half-applying.
    assert_eq!(graph.parent_of(2), Some(1));
}

#[test]
fn a_link_that_would_close_a_loop_is_refused_in_both_writers() {
    let mut graph = ScopeGraph::new();
    graph.bind(2, 1).unwrap();
    graph.bind(3, 2).unwrap();

    // Self-link is the degenerate case, and 1 is 2's own ancestor: both would make every chain
    // walk run forever, so both are refused rather than tolerated.
    assert_eq!(
        graph.bind(4, 4),
        Err(ScopeError::Cycle { key: 4, parent: 4 })
    );
    assert_eq!(
        graph.bind(1, 3),
        Err(ScopeError::Cycle { key: 1, parent: 3 })
    );

    // A refused bind wrote nothing: refusing must not leave a key half-linked.
    assert_eq!(graph.parent_of(1), None);
    assert_eq!(graph.parent_of(4), None);
}

#[test]
fn only_the_returned_binding_moves_a_link_and_the_move_keeps_the_cycle_check() {
    let mut graph = ScopeGraph::new();
    let mut binding = graph.bind(10, 1).unwrap();
    graph.bind(20, 2).unwrap();
    graph.bind(30, 10).unwrap();

    assert_eq!(binding.key(), 10);
    assert_eq!(binding.parent(), 1);

    // The recompose move: same key, new parent. The key's identity survives, so every listener
    // registered under it keeps routing — which is what the numeric encoding cannot do.
    binding.rebind(&mut graph, 20).unwrap();
    assert_eq!(graph.chain_of(Some(10)), vec![10, 20, 2]);
    assert_eq!(binding.parent(), 20);

    // The re-link runs the same cycle check as the bind: 30 descends from 10, so it may not
    // become 10's parent.
    assert_eq!(
        binding.rebind(&mut graph, 30),
        Err(ScopeError::Cycle {
            key: 10,
            parent: 30
        })
    );
    // A refused re-link leaves the binding usable rather than consuming the only handle.
    assert_eq!(binding.parent(), 20);
    assert_eq!(graph.chain_of(Some(10)), vec![10, 20, 2]);
}

#[test]
fn admission_flows_up_the_chain_never_down() {
    let mut graph = ScopeGraph::new();
    graph.bind(20, 10).unwrap();
    graph.bind(30, 20).unwrap();
    // A sibling subtree: same root, different branch.
    graph.bind(40, 10).unwrap();

    // A listener tagged with an ancestor receives a descendant's dispatch.
    assert!(graph.is_within(30, 30));
    assert!(graph.is_within(30, 20));
    assert!(graph.is_within(30, 10));

    // The reverse does not hold, and neither does the sibling: an agent must not hear another
    // agent's events through a shared root it happens to sit under.
    assert!(!graph.is_within(10, 30));
    assert!(!graph.is_within(20, 30));
    assert!(!graph.is_within(40, 30));
    assert!(!graph.is_within(30, 40));

    // Disjoint roots: neither admits the other.
    let mut other = ScopeGraph::new();
    other.bind(500, 400).unwrap();
    assert!(!graph.is_within(500, 10));
    assert!(!graph.is_within(30, 400));
}

#[test]
fn an_empty_graph_answers_every_admission_from_the_chain_alone() {
    let graph = ScopeGraph::new();
    assert!(graph.is_empty());
    assert_eq!(graph.len(), 0);
    // Every key is a root, so each admits only itself — and `is_within` must not panic or hang.
    assert!(graph.is_within(7, 7));
    assert!(!graph.is_within(7, 8));
}

// ---- consumed work ---------------------------------------------------------------------------

/// A completed ending: the one case that does *not* account for a claim.
fn completed() -> TurnEndReason {
    TurnEndReason::Completed
}

/// A blocked ending: the pre-step rejection that discarded the claimed messages.
fn blocked() -> TurnEndReason {
    TurnEndReason::Blocked
}

#[test]
fn a_no_op_turn_does_not_account_for_work() {
    // The balanced no-op: a turn opened and closed having claimed nothing. If this credited the
    // turn, every rejection and empty claim would read as finished work.
    let log = [
        Event::TurnStart { turn: 1 },
        Event::TurnEnd {
            turn: 1,
            reason: &completed(),
        },
    ];
    assert_eq!(fold_consumed_work(&log), ConsumedWork::default());
}

#[test]
fn a_turn_that_stepped_accounts_for_work_whatever_its_ending() {
    let log = [
        Event::TurnStart { turn: 1 },
        Event::StepStart { turn: 1 },
        Event::TurnEnd {
            turn: 1,
            reason: &completed(),
        },
    ];
    let work = fold_consumed_work(&log);
    // Reaching a model call is running the work, so even a `completed` end is an account here —
    // this is precisely what separates it from the no-op above.
    assert_eq!(work.end.map(|end| end.turn), Some(1));
    assert!(!work.dropped_unrun);
}

#[test]
fn a_claim_plus_an_ending_that_accounts_for_it_is_an_account() {
    // Claimed without stepping, then blocked: the pre-step rejection discarded the claimed
    // messages, so that input's ending is this turn's ending.
    let log = [
        Event::TurnStart { turn: 1 },
        Event::InboxSpliced {
            removed: Some(2),
            outcome: SpliceOutcome::Consumed,
            inserted: 0,
        },
        Event::TurnEnd {
            turn: 1,
            reason: &blocked(),
        },
    ];
    let work = fold_consumed_work(&log);
    assert_eq!(work.end.map(|end| end.turn), Some(1));
    assert!(accounts_for_claim(&blocked()));
}

#[test]
fn a_claim_that_completed_without_stepping_is_the_rewritten_away_case() {
    // Claimed, never stepped, ended `completed`: the claim was rewritten away, so the work the
    // turn took will never run and the turn must not be credited with it.
    let log = [
        Event::TurnStart { turn: 1 },
        Event::InboxSpliced {
            removed: Some(1),
            outcome: SpliceOutcome::Consumed,
            inserted: 0,
        },
        Event::TurnEnd {
            turn: 1,
            reason: &completed(),
        },
    ];
    assert_eq!(fold_consumed_work(&log), ConsumedWork::default());
}

#[test]
fn a_cancellation_of_unrun_work_is_accounted_for_by_no_turn() {
    // The case a fold over boundaries alone cannot see: work was accepted and then cancelled out
    // of the inbox before any turn opened over it. No `turn/end` describes it.
    let log = [
        Event::TurnStart { turn: 1 },
        Event::StepStart { turn: 1 },
        Event::TurnEnd {
            turn: 1,
            reason: &completed(),
        },
        Event::InboxSpliced {
            removed: Some(3),
            outcome: SpliceOutcome::Canceled,
            inserted: 0,
        },
    ];
    let work = fold_consumed_work(&log);
    // The turn still accounts for what it ran, and the later drop is reported beside it.
    assert_eq!(work.end.map(|end| end.turn), Some(1));
    assert!(work.dropped_unrun);
}

#[test]
fn a_cancellation_that_reinserts_keeps_the_work_pending() {
    // A replacement moves the work to a new identity rather than dropping it. Only a cancellation
    // that leaves nothing behind drops work, and reading the removal count alone would convict
    // an edit as a loss.
    let log = [Event::InboxSpliced {
        removed: Some(1),
        outcome: SpliceOutcome::Canceled,
        inserted: 1,
    }];
    assert!(!fold_consumed_work(&log).dropped_unrun);
}

#[test]
fn an_insertion_only_splice_is_not_a_claim() {
    // The absent `removedCount` and an explicit zero both mean nothing left the queue, so nothing
    // was claimed — an append must not credit the open turn.
    let log = [
        Event::TurnStart { turn: 1 },
        Event::InboxSpliced {
            removed: None,
            outcome: SpliceOutcome::Consumed,
            inserted: 4,
        },
        Event::InboxSpliced {
            removed: Some(0),
            outcome: SpliceOutcome::Consumed,
            inserted: 2,
        },
        Event::TurnEnd {
            turn: 1,
            reason: &blocked(),
        },
    ];
    assert_eq!(fold_consumed_work(&log), ConsumedWork::default());
}

#[test]
fn a_splice_outside_any_open_turn_claims_nothing() {
    // Claims are the loop's step-boundary reads and are always inside a turn. A splice with no
    // turn open is someone else's edit, and must not be attributed to a later turn.
    let log = [
        Event::InboxSpliced {
            removed: Some(5),
            outcome: SpliceOutcome::Consumed,
            inserted: 0,
        },
        Event::TurnStart { turn: 1 },
        Event::TurnEnd {
            turn: 1,
            reason: &blocked(),
        },
    ];
    assert_eq!(fold_consumed_work(&log), ConsumedWork::default());
}

#[test]
fn a_later_accounting_turn_clears_a_drop_that_preceded_it() {
    // Anything dropped before an accounting turn closed is that turn's own ending to report; only
    // a drop *after* the last account is still unaccounted for. Without this, every resume would
    // carry a stale drop from an earlier teardown.
    let log = [
        Event::InboxSpliced {
            removed: Some(1),
            outcome: SpliceOutcome::Canceled,
            inserted: 0,
        },
        Event::TurnStart { turn: 1 },
        Event::StepStart { turn: 1 },
        Event::TurnEnd {
            turn: 1,
            reason: &completed(),
        },
    ];
    let work = fold_consumed_work(&log);
    assert_eq!(work.end.map(|end| end.turn), Some(1));
    assert!(!work.dropped_unrun, "the drop preceded the accounting turn");
}

#[test]
fn the_latest_accounting_turn_wins() {
    // Only the most recent account is reported: a reader asking "what did this log finish?" means
    // the last turn that actually did something, not the first.
    let log = [
        Event::TurnStart { turn: 1 },
        Event::StepStart { turn: 1 },
        Event::TurnEnd {
            turn: 1,
            reason: &completed(),
        },
        Event::TurnStart { turn: 2 },
        Event::StepStart { turn: 2 },
        Event::TurnEnd {
            turn: 2,
            reason: &blocked(),
        },
    ];
    let work = fold_consumed_work(&log);
    let end = work
        .end
        .expect("turn 2 closed over a step and must be the reported account");
    assert_eq!(end.turn, 2);
    assert_eq!(end.reason, blocked());
}

#[test]
fn a_dropped_unrun_flag_survives_a_turn_that_does_not_account() {
    // A `completed` no-op turn after a cancellation neither hides the loss nor claims it: the fold
    // must not treat "a turn ended" as "the loss is now explained".
    let log = [
        Event::InboxSpliced {
            removed: Some(2),
            outcome: SpliceOutcome::Canceled,
            inserted: 0,
        },
        Event::TurnStart { turn: 1 },
        Event::TurnEnd {
            turn: 1,
            reason: &completed(),
        },
    ];
    let work = fold_consumed_work(&log);
    assert_eq!(work.end, None);
    assert!(work.dropped_unrun);
}

#[test]
fn every_ending_except_completed_accounts_for_a_claim() {
    // Pinned as a table because the fallback is `true` on purpose: an unnameable backend-added
    // ending over consumed input must not read as success.
    assert!(!accounts_for_claim(&TurnEndReason::Completed));
    assert!(accounts_for_claim(&TurnEndReason::Blocked));
    assert!(accounts_for_claim(&TurnEndReason::Budget));
    assert!(accounts_for_claim(&TurnEndReason::Aborted {
        cause: "user".to_owned(),
    }));
    assert!(accounts_for_claim(&TurnEndReason::Failed {
        message: "boom".to_owned(),
        code: "E_TEST".to_owned(),
    }));
}

#[test]
fn an_empty_log_accounts_for_nothing() {
    assert_eq!(fold_consumed_work(&[]), ConsumedWork::default());
}
