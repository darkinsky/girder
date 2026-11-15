//! The tool budget as a stored setting, and the watcher that notices when it changes.
//!
//! [`crate::tool_limits`] owns the budget itself: [`ToolTracker`](crate::ToolTracker) copies its
//! [`ToolLimit`](crate::ToolLimit) at construction, and nothing in that module reads a setting. That
//! separation is right — the tracker is arithmetic and a tracker that reached for a store could not
//! be tested without one — but it leaves a gap every deployment hits: a write to
//! `tools.maxCalls` reaches the store and nothing else. A tracker built before the write keeps its
//! old ceiling for the rest of its life, so a long-lived loop that builds one tracker per session
//! never sees a configuration change at all. The `config_reload_e2e` integration test pins exactly
//! that: only a tracker constructed *after* the write reflects it.
//!
//! This module is the missing half. It does not touch the tracker: it reads the store, says whether
//! the stored configuration moved, and hands back the budget a caller should build a new tracker
//! from. A caller keeps its tracker in whatever field it already has and rebuilds it when
//! [`Watcher::refresh`] returns `true`.
//!
//! # Why the answer is "did it change" rather than "here is the value"
//!
//! Reading the store is cheap; rebuilding a tracker is not. A [`Tracker`](crate::ToolTracker) holds
//! every count of the turn so far, so replacing it to pick up a new ceiling throws away the counts a
//! person diagnosing a runaway turn needs, and doing it on *every* call would reset the budget on
//! every call — the ceiling would never be reached. [`refresh`](Watcher::refresh) therefore answers
//! the one question a caller has to act on: nothing moved (`false`, keep the tracker you have) or
//! something did (`true`, rebuild). It reads the store either way, so the answer is always current.
//!
//! # Why a version number
//!
//! A caller that logs the change, or a dashboard that shows it, needs something stable to name it,
//! and a budget with only a number in it cannot tell "3, as it always was" from "3, after a
//! round trip through an invalid value". Every detected change increments
//! [`Budget::version`], so a rebuild is traceable to one step. It is deliberately a counter of
//! changes *this watcher observed*, not a value read from the store: [`SettingsStore`] keeps no
//! revisions, and inventing one would make this module depend on a field the seam does not have.
//! Comparing the incoming configuration against the one last read is what detects a change;
//! the version is what records it.
//!
//! # Why an invalid value falls back *and* is reported
//!
//! Falling back silently is the worse of the two failures. A deployment that writes
//! `"three"`, `-1`, or `1.5` under `tools.maxCalls` gets the default budget and no signal at all,
//! so the setting it believes is in force is not, and the only way to notice is to run out of calls.
//! Refusing the value instead would be the other extreme: a single bad row would stop the turn
//! rather than bound it. So the value falls back to [`DEFAULT_MAX_CALLS`], the change still counts as
//! a change (the effective budget may be unchanged, the *configuration* is not), and
//! [`Watcher::warnings`] carries one entry naming the key, the value found, and the ceiling used.
//! A caller that never reads it loses nothing; a caller that logs it can see what happened.
//!
//! # What is watched, and what that costs
//!
//! The snapshot is the stored value of every key this module reads — today exactly
//! [`MAX_CALLS_KEY`]. Any other setting a deployment writes does not bump the version, so
//! `provider.model` changing does not rebuild a tracker. The comparison is on the stored value
//! rather than on the resolved number on purpose: an invalid value replaced by a valid one that
//! happens to resolve to the same count is still a change, because the warning it produced went
//! away.
//!
//! Reading the whole store and hashing it would watch keys this module has no opinion about, and
//! would report a change for every unrelated write — the rebuild-per-call cost this watcher exists
//! to avoid. A second watched key is a field on the snapshot, not a hash of the store.

// The crate root is wired up separately, so until then not every item here is referenced by the
// library. The allow is scoped to this module and disappears once it is exported.
#![allow(dead_code)]

use std::fmt;
use std::sync::Arc;

use serde_json::Value;

// Named through the crate alias rather than `crate::`: the tests include this file by path, and a
// `crate::` path would there resolve to a second, structurally identical `SettingsStore` that no
// exported store implements. Through the alias both builds see the one trait the crate exports.
use girder_core::{SettingsStore, ToolLimit};

/// The settings key that holds the per-tool call ceiling.
pub const MAX_CALLS_KEY: &str = "tools.maxCalls";

/// The call ceiling used when the store holds no usable value for [`MAX_CALLS_KEY`].
///
/// Three, from the stored-setting reference reader in `config_reload_e2e`: low enough that a loop
/// resending one call is caught within a few steps, high enough that reading two files still fits.
/// It is deliberately not [`ToolLimit::default`]'s forty — a deployment that *configured* a ceiling
/// is asking for a bound, and one that wants forty stores forty.
pub const DEFAULT_MAX_CALLS: u32 = 3;

/// How much of a bad stored value a [`Warning`] repeats.
///
/// Bounded because the warning is meant for a log line: a store may hold a megabyte under the key,
/// and repeating all of it would bury the one fact that matters, which is that it is not a number.
const MAX_RENDERED_VALUE_CHARS: usize = 40;

/// One budget, and the version of the configuration it was read from.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Budget {
    /// How many calls of one tool a turn may make.
    ///
    /// `0` is a hard stop rather than "unlimited", matching [`ToolLimit`]: a tool a deployment may
    /// not call at all is a configuration it might want, and reading it as unlimited would invert it.
    pub max_calls: u32,
    /// How many changes this watcher has observed since it was built.
    ///
    /// `0` is the initial read. Each [`Watcher::refresh`] that finds a different stored
    /// configuration adds one, so a caller can name the rebuild in a log or a metric.
    pub version: u64,
}

/// Something about the stored configuration a caller should know about.
///
/// One variant today, because one key is all this module reads. The type is an enum rather than a
/// string so a caller can match on the cause instead of parsing a message, and so the second key
/// that needs reporting is a variant rather than a message format.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Warning {
    /// [`MAX_CALLS_KEY`] holds something that cannot be read as a call count.
    ///
    /// The default is in effect; this says so, and says what was found instead.
    InvalidMaxCalls {
        /// The key the value was found under.
        key: String,
        /// The stored value, rendered as JSON and shortened to [`MAX_RENDERED_VALUE_CHARS`].
        stored: String,
        /// The ceiling actually in use.
        used: u32,
    },
}

impl Warning {
    /// The settings key the warning is about.
    #[must_use]
    pub fn key(&self) -> &str {
        match self {
            Self::InvalidMaxCalls { key, .. } => key,
        }
    }
}

impl fmt::Display for Warning {
    /// Written as a full sentence: a warning is read in a log next to lines that are not warnings.
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidMaxCalls { key, stored, used } => write!(
                formatter,
                "`{key}` holds {stored}, which is not a call count, so the default of {used} calls \
                 is in effect instead"
            ),
        }
    }
}

/// The stored configuration this module watches.
///
/// Kept as the raw stored value rather than as the resolved number, so a value that changes without
/// changing the effective budget is still visible; see the module docs.
#[derive(Clone, Debug, PartialEq)]
struct Snapshot {
    /// What the store holds for [`MAX_CALLS_KEY`], if anything.
    max_calls: Option<Value>,
}

impl Snapshot {
    /// Reads the watched keys out of `store`.
    fn read(store: &dyn SettingsStore) -> Self {
        Self {
            max_calls: store.get(MAX_CALLS_KEY),
        }
    }
}

/// Where a [`Watcher`] reads from.
///
/// Two variants because the two callers differ. A test or a one-shot check borrows the store it
/// already owns; a loop that outlives the call that built it cannot borrow, and needs the watcher to
/// hold the store itself. Both read through the same trait, so nothing else in this module has to
/// know which one it got.
enum Source<'a> {
    /// Borrowed from the caller, as [`Watcher::new`] takes it.
    Borrowed(&'a dyn SettingsStore),
    /// Shared and owned, as [`Watcher::from_arc`] takes it.
    Owned(Arc<dyn SettingsStore>),
}

impl Source<'_> {
    /// The store this watcher reads from.
    fn store(&self) -> &dyn SettingsStore {
        match self {
            Self::Borrowed(store) => *store,
            Self::Owned(store) => &**store,
        }
    }
}

/// A budget read from a store, with the ability to notice that the store changed.
///
/// Construct it once, keep it beside the tracker, and call [`refresh`](Self::refresh) before each
/// turn — or before each call, which is the same cost as reading one setting. A `true` means the
/// stored configuration moved and the tracker should be rebuilt from [`limit`](Self::limit); a
/// `false` means the tracker in hand is still the right one, counts and all.
pub struct Watcher<'a> {
    /// The budget as of the last read or refresh.
    current: Budget,
    /// Where the next read comes from.
    source: Source<'a>,
    /// The stored configuration the last read or refresh saw.
    snapshot: Snapshot,
    /// What was wrong with it; empty when everything read cleanly.
    warnings: Vec<Warning>,
}

impl<'a> Watcher<'a> {
    /// Reads the initial budget from `store`.
    ///
    /// The read happens here, so a watcher built after a write starts at the written value and a
    /// watcher built before it does not — which is the whole point of [`refresh`](Self::refresh).
    /// Nothing is cached beyond the one snapshot the next refresh compares against, so a caller that
    /// builds a watcher and never refreshes behaves exactly like a caller that reads once.
    #[must_use]
    pub fn new(store: &'a dyn SettingsStore) -> Self {
        Self::from_source(Source::Borrowed(store))
    }

    /// Reads the initial budget from a store this watcher will own.
    ///
    /// Same contract as [`new`](Self::new), for a caller that cannot lend the store for as long as a
    /// watcher lives — a session or a turn that outlives the call that constructed it. The `Arc` is
    /// the store's own sharing contract, so the watcher adds no locking of its own.
    #[must_use]
    pub fn from_arc(store: Arc<dyn SettingsStore>) -> Watcher<'static> {
        Watcher::from_source(Source::Owned(store))
    }

    /// Reads the current configuration and reports whether it differs from the last read.
    ///
    /// Returns `true` only when the stored configuration changed, so a caller that rebuilds its
    /// tracker on `true` rebuilds it once per change rather than once per call. A `false` leaves the
    /// budget, the version, and the warnings untouched — nothing happened, so nothing is reported —
    /// and an unchanged store therefore costs one `get` and nothing else.
    ///
    /// A changed configuration is applied before the return, so
    /// [`current`](Self::current) and [`warnings`](Self::warnings) describe the same read that
    /// produced the `true`. An invalid value does not make this return `false`: the stored
    /// configuration did change, and the warning it produced may be the only signal the caller gets.
    #[must_use]
    pub fn refresh(&mut self) -> bool {
        let snapshot = Snapshot::read(self.source.store());
        if snapshot == self.snapshot {
            return false;
        }
        let (max_calls, warning) = resolve(snapshot.max_calls.as_ref());
        self.current = Budget {
            max_calls,
            // Saturating because a version is a diagnostic, not a ledger: a watcher that somehow saw
            // 2^64 changes should keep reporting "changed" rather than panic on the last one.
            version: self.current.version.saturating_add(1),
        };
        self.snapshot = snapshot;
        self.warnings = warning.into_iter().collect();
        true
    }

    /// The budget as of the last read or refresh.
    #[must_use]
    pub fn current(&self) -> &Budget {
        &self.current
    }

    /// What was wrong with the configuration the last read or refresh saw.
    ///
    /// Empty when it read cleanly, which is the ordinary case: the default that applies when the key
    /// is absent is not a warning. Replaced wholesale by each refresh that finds a change, so a value
    /// fixed after being invalid clears its warning instead of leaving a stale one behind.
    #[must_use]
    pub fn warnings(&self) -> &[Warning] {
        &self.warnings
    }

    /// The tracker limit this budget implies.
    ///
    /// One stored number governs both ceilings because `tools.maxCalls` is the only key read: a
    /// deployment that caps a turn at three calls wanted a turn that stops after three, and a
    /// consecutive-failure ceiling above that can never be reached. A
    /// `tools.maxConsecutiveFailures` key is where the two would split.
    #[must_use]
    pub fn limit(&self) -> ToolLimit {
        ToolLimit::new(self.current.max_calls, self.current.max_calls)
    }
}

impl fmt::Debug for Watcher<'_> {
    /// Shows the configuration, not the store.
    ///
    /// [`SettingsStore`] has no `Debug` bound, and a watcher's interesting state is the budget it
    /// resolved and what it had to complain about; the store is a seam, not a value.
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("Watcher")
            .field("current", &self.current)
            .field("warnings", &self.warnings)
            .finish_non_exhaustive()
    }
}

/// Resolves the stored value into a ceiling, plus a warning when it had to be ignored.
///
/// Absent and unusable are different answers even though both use the default: absent is what an
/// unconfigured store looks like and needs no comment, while a value that cannot be read means
/// something a person wrote is not doing what they think.
fn resolve(stored: Option<&Value>) -> (u32, Option<Warning>) {
    let Some(value) = stored else {
        return (DEFAULT_MAX_CALLS, None);
    };
    // A JSON number that is not an integer, a negative number, a string, and `null` all fail here,
    // as does an integer too large for `u32`. All four mean the same thing to a caller: this is not
    // a call count, so the default applies and the value is named in the warning.
    match value.as_u64().and_then(|count| u32::try_from(count).ok()) {
        Some(max_calls) => (max_calls, None),
        None => (
            DEFAULT_MAX_CALLS,
            Some(Warning::InvalidMaxCalls {
                key: MAX_CALLS_KEY.to_owned(),
                stored: render(value),
                used: DEFAULT_MAX_CALLS,
            }),
        ),
    }
}

/// Renders a stored value for a warning, shortened to [`MAX_RENDERED_VALUE_CHARS`] characters.
///
/// Truncated by characters rather than bytes: slicing a UTF-8 string at a byte offset can panic on a
/// multi-byte character, and a value that is not a call count is exactly the kind of value that might
/// be text a user typed.
fn render(value: &Value) -> String {
    let text = value.to_string();
    let mut rendered: String = text.chars().take(MAX_RENDERED_VALUE_CHARS).collect();
    if rendered.chars().count() < text.chars().count() {
        rendered.push('…');
    }
    rendered
}
