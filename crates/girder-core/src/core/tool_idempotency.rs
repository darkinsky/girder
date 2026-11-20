//! Deciding whether a tool call that a crash interrupted may run again.
//!
//! # What a crash actually leaves behind
//!
//! `agent_loop` writes a `tool/call` event *before* the tool body is dispatched and a `tool/result`
//! event *after* it returns. A process that dies in between therefore leaves exactly one durable
//! fact: the call was started. That fact says nothing about whether the body finished, and the body
//! is usually the part with the side effect — half a file written, half a `git` command applied.
//!
//! So the two questions a restart asks have different answers:
//!
//! - *Did this call start?* The `tool/call` record answers that, and it is written before anything
//!   can go wrong.
//! - *Did this call finish?* Only a `tool/result` answers that, and its absence is **not** evidence
//!   that nothing happened. It is evidence that no one observed the end.
//!
//! That asymmetry is the whole reason this module exists: a recovery path that reads "no result" as
//! "did not run" would replay a `write` that had already truncated the file, or a `bash` that had
//! already pushed. Replaying a read costs a syscall; replaying a write can cost the user's work.
//!
//! # Why the log is the only evidence
//!
//! Nothing in memory outlives the process: `dispatch` keeps no journal, and `session_resume_rpc`
//! deliberately refuses to append a synthetic result for an interrupted call rather than hand the
//! loop a fabricated observation. A restarted host reads a log in which the call has no answer, and
//! has to decide what to do with it. This module is that decision, expressed as a pure function of
//! the event log so it can be tested without a loop, a session, or a clock.
//!
//! # Why the safe list is short
//!
//! [`Idempotency::Safe`] is an allowlist, not a denylist of writers. A new tool is unknown, and the
//! cost of guessing wrong is asymmetric in one direction: treating a writer as safe repeats a side
//! effect the user cannot undo, while treating a reader as unsafe only asks a human one question.
//! Every unclassified tool is therefore [`Idempotency::Unknown`], and both `Unknown` and `Unsafe`
//! resolve to [`Decision::Confirm`] on a retry.
//!
//! # Why the fingerprint is the raw arguments
//!
//! A call's identity is its id *and* its arguments. Provider-issued ids are unique per response, not
//! per session, and a damaged or replayed log can carry one id twice with different arguments; a
//! module that matched on the id alone would either skip a write that never happened or repeat one
//! that already did. The encoding is the raw argument text with length prefixes rather than a hash,
//! for the reason `tool_cache` gives for its own key: a collision would silently merge two different
//! calls, which is exactly the failure this module is meant to prevent. The cost is that a record
//! holds the arguments it saw, which is bounded by the size of the call the model made.
//!
//! Fingerprints identify a call *within one log*. They do not claim two differently formatted JSON
//! objects are the same arguments: a retry after a crash replays the recorded call verbatim, so byte
//! equality is both sufficient and the strongest claim that can be made without a JSON parse.

use std::fmt::Write as _;

use ::girder_core::SessionEvent;

/// Tools whose re-execution a user cannot observe.
///
/// Read-only and cheap: a repeated `read`, `glob`, or `grep` returns the same view of the workspace
/// (possibly a newer one) and leaves nothing behind, so re-running one after a crash is always
/// preferable to asking a human about it.
const SAFE_TOOLS: &[&str] = &["read", "glob", "grep"];

/// Tools whose re-execution repeats a side effect.
///
/// `write` truncates and rewrites, `edit` applies a patch, and `bash` runs an arbitrary program. All
/// three can succeed without a result being written — the crash happens between the effect and the
/// append — which is why they need a human rather than a retry.
const UNSAFE_TOOLS: &[&str] = &["write", "edit", "bash"];

/// Which side of the line a tool falls on.
///
/// Three values rather than a boolean because "no" and "we do not know" call for different handling
/// in the reason a human reads, even though both currently stop at [`Decision::Confirm`]. Collapsing
/// them would lose the distinction the moment a policy wants to auto-approve one but not the other.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Idempotency {
    /// Repeating the call is safe: it is a read.
    Safe,

    /// Repeating the call repeats a side effect.
    Unsafe,

    /// Not classified. Treated as unsafe, because the alternative is guessing.
    Unknown,
}

/// One call's record, folded out of an event stream.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Executed {
    /// The provider-issued call id, as the log spells it.
    pub call_id: String,

    /// The tool the call named, from the first record of the id.
    pub tool: String,

    /// The call's arguments, encoded by [`fingerprint`].
    ///
    /// Taken from the first record of the id: that is the invocation the log established, and a
    /// later record under the same id with different arguments is a different call, not a retry
    /// (see [`decide`]).
    pub fingerprint: String,

    /// How many `tool/call` records this id has, saturated at [`u32::MAX`].
    ///
    /// An attempt is a recorded dispatch, so this counts starts and not completions: a call that was
    /// dispatched twice and never answered has `attempts == 2`. A `tool/result` proves that at least
    /// one of those attempts finished, which is why the count alone never answers "did it run" —
    /// [`decide`] combines it with the presence of a result.
    pub attempts: u32,
}

/// What the caller should do with a call it is about to make.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Decision {
    /// Execute: either this call has never been seen, or re-running it costs nothing.
    Run,

    /// Do not execute: an earlier run of this same call already has a result, and running it again
    /// would repeat whatever it did.
    Skip {
        /// Why, in the form a log line or a model-facing message can carry.
        reason: String,
    },

    /// Do not execute without a human's answer: this call started, has no result, and is not known
    /// to be safe to repeat.
    Confirm {
        /// Why, naming the tool, the call, and the number of attempts.
        reason: String,
    },
}

/// A tool call's arguments, encoded so that two different calls cannot encode identically.
///
/// # Why length prefixes
///
/// No separator is safe: every candidate character can occur inside a tool name or an argument
/// string, so `read` + `{"path":"a"}` would encode the same as `read{"path"}` + `"a"`. Prefixing
/// each field with its byte length makes the boundaries unambiguous by construction.
///
/// # Contract
///
/// Every other function in this module encodes with exactly this function, so a caller comparing
/// against a recorded [`Executed::fingerprint`] must build its input with it too. A hand-built string
/// does not collide with a real fingerprint; it simply never matches one.
#[must_use]
pub fn fingerprint(tool: &str, arguments: &str) -> String {
    let mut out = String::with_capacity(tool.len() + arguments.len() + 24);
    // Writing into a `String` cannot fail; the result is dropped rather than unwrapped so this
    // module keeps its promise of having no panic path.
    let _ = write!(out, "{}:{tool}{}:{arguments}", tool.len(), arguments.len());
    out
}

/// Whether a tool may be re-run without repeating a side effect.
///
/// Exact, case-sensitive matching: a tool registry names tools in lowercase, and a near-miss (a
/// hypothetical `Read`) is a tool this module has never seen, so it is [`Idempotency::Unknown`]
/// rather than a guessed [`Idempotency::Safe`].
///
/// # Why an allowlist
///
/// See the module docs: the safe list is short on purpose, and every tool added to this crate lands
/// in `Unknown` until someone classifies it deliberately.
#[must_use]
pub fn idempotency(tool: &str) -> Idempotency {
    if SAFE_TOOLS.contains(&tool) {
        Idempotency::Safe
    } else if UNSAFE_TOOLS.contains(&tool) {
        Idempotency::Unsafe
    } else {
        Idempotency::Unknown
    }
}

/// Rebuilds the calls an event stream records, one entry per call id, in first-appearance order.
///
/// A `tool/result` with no `tool/call` before it is skipped: it has no name and no arguments, so no
/// record can be built from it. [`decide`] still treats such a result as evidence that the call was
/// answered, because a log that lost a call is exactly the situation in which a silent retry must not
/// happen.
///
/// Ordering is the log's order rather than sorted, so a caller that reports the result reads it in
/// the sequence the session recorded.
#[must_use]
pub fn executed(events: &[SessionEvent]) -> Vec<Executed> {
    let mut records: Vec<Executed> = Vec::new();

    for event in events {
        let SessionEvent::ToolCall {
            id,
            name,
            arguments,
        } = event
        else {
            continue;
        };

        let call_id = id.as_str();
        // Linear search over the calls of one session, not a map: a turn has a handful of calls and
        // the scan is over an already-materialized slice, so a hash table would cost more than it
        // saves and would lose the log order this function promises.
        if let Some(record) = records.iter_mut().find(|record| record.call_id == call_id) {
            record.attempts = record.attempts.saturating_add(1);
            // The fingerprint of the first record is kept: a second record under the same id with
            // different arguments is a different call, and `decide` refuses to treat it as a retry.
        } else {
            records.push(Executed {
                call_id: call_id.to_owned(),
                tool: name.clone(),
                fingerprint: self::fingerprint(name, arguments),
                attempts: 1,
            });
        }
    }

    records
}

/// Whether a call may be executed now.
///
/// `fingerprint` must come from [`fingerprint()`] with the arguments this call is about to use;
/// anything else never matches the log and is treated as a new call.
///
/// # The rules, in the order they are applied
///
/// 1. Nothing recorded for this id — `Run`. There is no earlier effect to repeat.
/// 2. A result exists for this id but the call record is missing — `Skip`. The call is written before
///    the body runs, so this shape means the log lost a record, and the only safe reading of a lost
///    record is that the call may have run.
/// 3. The id was recorded with different arguments — that is not this call (see the module docs).
///    `Run` for a safe tool, [`Decision::Confirm`] otherwise.
/// 4. The id was recorded with these arguments and a result exists — `Skip`. This is the case the
///    module exists for: the call already produced an observation, and repeating it would repeat the
///    side effect and give the model two answers for one question.
/// 5. The id was recorded with these arguments and no result — the process died between the writes.
///    `Run` for a safe tool, [`Decision::Confirm`] otherwise: the body may have finished without its
///    result, and no one can tell from the log.
#[must_use]
pub fn decide(events: &[SessionEvent], tool: &str, call_id: &str, fingerprint: &str) -> Decision {
    let mut recorded_fingerprint: Option<String> = None;
    let mut result_at: Option<usize> = None;
    let mut attempts: u32 = 0;

    for (at, event) in events.iter().enumerate() {
        match event {
            SessionEvent::ToolCall {
                id,
                name,
                arguments,
            } if id.as_str() == call_id => {
                attempts = attempts.saturating_add(1);
                if recorded_fingerprint.is_none() {
                    recorded_fingerprint = Some(self::fingerprint(name, arguments));
                }
            }
            // The first result is the one reported: a second result for the same call is a log
            // contradiction, and naming the earliest one points a reader at the moment it went wrong.
            SessionEvent::ToolResult { id, .. }
                if id.as_str() == call_id && result_at.is_none() =>
            {
                result_at = Some(at);
            }
            _ => {}
        }
    }

    let Some(recorded) = recorded_fingerprint else {
        return match result_at {
            Some(at) => Decision::Skip {
                reason: format!(
                    "call `{call_id}` has a result at event {at} but no recorded call; the log lost \
                     the call, and a call whose record is lost may still have run"
                ),
            },
            None => Decision::Run,
        };
    };

    // A different argument set under the same id is a different call. Treating the two as one would
    // let the earlier result answer this question, which is the wrong answer to give the model.
    if recorded != fingerprint {
        return match idempotency(tool) {
            Idempotency::Safe => Decision::Run,
            Idempotency::Unsafe | Idempotency::Unknown => Decision::Confirm {
                reason: format!(
                    "call `{call_id}` is already recorded for `{tool}` with different arguments; \
                     the earlier run does not answer this one, and `{tool}` is not known to be safe \
                     to run twice"
                ),
            },
        };
    }

    if let Some(at) = result_at {
        return Decision::Skip {
            reason: format!("call `{call_id}` for `{tool}` already has a result at event {at}"),
        };
    }

    match idempotency(tool) {
        Idempotency::Safe => Decision::Run,
        Idempotency::Unsafe => Decision::Confirm {
            reason: format!(
                "call `{call_id}` dispatched `{tool}` {attempts} time(s) with no recorded result; \
                 re-running it may repeat a side effect that already happened"
            ),
        },
        Idempotency::Unknown => Decision::Confirm {
            reason: format!(
                "call `{call_id}` dispatched `{tool}` {attempts} time(s) with no recorded result, \
                 and `{tool}` is not known to be idempotent; re-running it cannot be proven safe"
            ),
        },
    }
}
