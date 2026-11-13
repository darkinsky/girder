//! Tests for stage-ordered boot tracing.
//!
//! The distinction under test is *sequence*, not just bookkeeping: a fatal stage must stop the
//! sequence, and the report must let a reader see which stage degraded and why without correlating
//! several log lines. Every test therefore asserts on the trace's own answer — should it continue,
//! where did it stop — rather than on the wrapping text alone.
//!
//! The module is compiled into this test binary through `#[path]` because exporting it from `lib.rs`
//! is a separate change. When it is exported, this can become
//! `use girder_core::boot_stages::{BootTrace, Stage, StageOutcome};`.

#[path = "../src/boot_stages.rs"]
mod boot_stages;

use boot_stages::{BootTrace, Stage, StageOutcome};

// ---- a clean start-up ---------------------------------------------------------------------------

#[test]
fn every_stage_ok_reports_continue() {
    let mut trace = BootTrace::new();
    for stage in Stage::ALL {
        trace.record(stage, StageOutcome::Ok);
    }

    assert!(trace.should_continue());
    assert_eq!(trace.blocker(), None);
    assert!(trace.degradations().is_empty());

    let report = trace.report();
    for stage in Stage::ALL {
        assert!(
            report.contains(&format!("{}: ok", stage.label())),
            "missing stage line for {stage}: {report}"
        );
    }
    assert!(report.contains("verdict: continue"), "{report}");
    assert!(!report.contains("DEGRADED"), "{report}");
    assert!(!report.contains("FATAL"), "{report}");
}

// ---- a degraded stage ---------------------------------------------------------------------------

#[test]
fn a_degraded_stage_is_named_with_its_reason_and_does_not_stop() {
    let mut trace = BootTrace::new();
    trace.record(Stage::Configure, StageOutcome::Ok);
    trace.record(Stage::MountServices, StageOutcome::Ok);
    trace.record(
        Stage::LoadExtensions,
        StageOutcome::degraded("plugin `acme/tools` failed to activate"),
    );
    trace.record(Stage::RestoreState, StageOutcome::Ok);
    trace.record(Stage::Listen, StageOutcome::Ok);

    assert!(trace.should_continue(), "a degradation is not a blocker");
    assert_eq!(trace.blocker(), None);

    let report = trace.report();
    assert!(
        report.contains("load-extensions: DEGRADED (plugin `acme/tools` failed to activate)"),
        "the report must name the degraded stage and why: {report}"
    );
    assert!(
        report.contains("verdict: continue; degraded: load-extensions"),
        "the verdict must summarise the degradation: {report}"
    );

    let degradations = trace.degradations();
    assert_eq!(degradations.len(), 1);
    assert_eq!(degradations[0].0, Stage::LoadExtensions);
    assert!(degradations[0].1.contains("acme/tools"));
}

// ---- a fatal stage ------------------------------------------------------------------------------

#[test]
fn a_fatal_stage_stops_the_sequence_and_becomes_the_blocker() {
    let mut trace = BootTrace::new();
    trace.record(Stage::Configure, StageOutcome::Ok);
    trace.record(
        Stage::MountServices,
        StageOutcome::fatal("session store refused to open"),
    );

    assert!(!trace.should_continue(), "start-up must not continue");
    assert_eq!(trace.blocker(), Some(Stage::MountServices));

    let report = trace.report();
    assert!(
        report.contains("mount-services: FATAL (session store refused to open)"),
        "{report}"
    );
    assert!(
        report.contains("verdict: stopped at mount-services"),
        "{report}"
    );
    // Stages after the blocker never ran, and the report says so rather than omitting them.
    assert!(report.contains("restore-state: not reached"), "{report}");
    assert!(report.contains("listen: not reached"), "{report}");
    // A stage before the blocker that never ran reads differently from one after it.
    assert!(report.contains("configure: ok"), "{report}");
}

#[test]
fn the_earliest_fatal_stage_is_the_blocker_regardless_of_record_order() {
    let mut trace = BootTrace::new();
    // Recorded out of boot order on purpose: a replayed or interleaved log must still blame the
    // stage the sequence could not get past.
    trace.record(Stage::Listen, StageOutcome::fatal("port already in use"));
    trace.record(
        Stage::Configure,
        StageOutcome::fatal("config is not valid yaml"),
    );

    assert_eq!(trace.blocker(), Some(Stage::Configure));
    assert!(!trace.should_continue());
    assert!(trace.report().contains("verdict: stopped at configure"));
}

#[test]
fn a_record_after_a_fatal_stage_does_not_unblock_it() {
    let mut trace = BootTrace::new();
    trace.record(Stage::Configure, StageOutcome::fatal("config unreadable"));
    assert!(!trace.should_continue());

    // A misbehaving runner that keeps going gets its records kept, but the verdict does not change.
    trace.record(Stage::MountServices, StageOutcome::Ok);
    trace.record(Stage::Listen, StageOutcome::Ok);

    assert!(!trace.should_continue());
    assert_eq!(trace.blocker(), Some(Stage::Configure));
}

// ---- ordering -----------------------------------------------------------------------------------

#[test]
fn stages_are_ordered_by_boot_position() {
    assert!(Stage::Configure < Stage::MountServices);
    assert!(Stage::MountServices < Stage::LoadExtensions);
    assert!(Stage::LoadExtensions < Stage::RestoreState);
    assert!(Stage::RestoreState < Stage::Listen);
    assert!(Stage::Configure < Stage::Listen);

    let mut shuffled = vec![Stage::Listen, Stage::Configure, Stage::LoadExtensions];
    shuffled.sort();
    assert_eq!(
        shuffled,
        vec![Stage::Configure, Stage::LoadExtensions, Stage::Listen]
    );

    // `ALL` is the boot sequence, which the report relies on.
    let mut sorted = Stage::ALL.to_vec();
    sorted.sort();
    assert_eq!(sorted, Stage::ALL.to_vec());
}

#[test]
fn records_keep_the_order_they_were_made() {
    let mut trace = BootTrace::new();
    trace.record(Stage::Configure, StageOutcome::Ok);
    trace.record(Stage::MountServices, StageOutcome::degraded("no sandbox"));
    trace.record(Stage::LoadExtensions, StageOutcome::Ok);

    let recorded: Vec<Stage> = trace.records().iter().map(|(stage, _)| *stage).collect();
    assert_eq!(
        recorded,
        vec![
            Stage::Configure,
            Stage::MountServices,
            Stage::LoadExtensions
        ]
    );
}

// ---- repeats ------------------------------------------------------------------------------------

#[test]
fn repeating_a_stage_keeps_the_worst_outcome_and_the_whole_history() {
    let mut trace = BootTrace::new();
    trace.record(
        Stage::RestoreState,
        StageOutcome::degraded("snapshot missing"),
    );
    trace.record(Stage::RestoreState, StageOutcome::Ok);

    // A successful retry does not erase the degradation that preceded it.
    assert!(
        trace
            .outcome(Stage::RestoreState)
            .is_some_and(StageOutcome::is_degraded)
    );
    assert!(trace.report().contains("restore-state: DEGRADED"));
    assert_eq!(trace.records().len(), 2, "both attempts stay visible");

    // The same rule holds for a fatal outcome, which a later success cannot mask.
    trace.record(Stage::RestoreState, StageOutcome::fatal("disk gone"));
    assert!(!trace.should_continue());
    assert_eq!(trace.blocker(), Some(Stage::RestoreState));

    // A later, milder record still does not downgrade a fatal one.
    trace.record(Stage::RestoreState, StageOutcome::degraded("partial"));
    assert!(!trace.should_continue());
    assert_eq!(trace.blocker(), Some(Stage::RestoreState));
}

#[test]
fn repeating_an_ok_stage_stays_ok() {
    let mut trace = BootTrace::new();
    trace.record(Stage::Listen, StageOutcome::Ok);
    trace.record(Stage::Listen, StageOutcome::Ok);

    assert!(trace.should_continue());
    assert!(
        trace
            .outcome(Stage::Listen)
            .is_some_and(StageOutcome::is_ok)
    );
    assert_eq!(trace.records().len(), 2);
}

// ---- the empty trace ----------------------------------------------------------------------------

#[test]
fn an_empty_trace_says_so_and_does_not_block() {
    let trace = BootTrace::new();

    assert_eq!(trace.report(), "boot: no stages recorded\n");
    assert!(
        trace.should_continue(),
        "nothing attempted is not a failure"
    );
    assert_eq!(trace.blocker(), None);
    assert!(trace.records().is_empty());
    assert_eq!(trace.outcome(Stage::Configure), None);
    assert!(trace.degradations().is_empty());
}

// ---- outcome helpers ----------------------------------------------------------------------------

#[test]
fn outcome_accessors_describe_the_variant() {
    assert!(StageOutcome::Ok.is_ok());
    assert_eq!(StageOutcome::Ok.reason(), None);

    let degraded = StageOutcome::degraded("slow disk");
    assert!(degraded.is_degraded());
    assert!(!degraded.is_fatal());
    assert_eq!(degraded.reason(), Some("slow disk"));

    let fatal = StageOutcome::fatal("no config");
    assert!(fatal.is_fatal());
    assert!(!fatal.is_degraded());
    assert_eq!(fatal.reason(), Some("no config"));
}
