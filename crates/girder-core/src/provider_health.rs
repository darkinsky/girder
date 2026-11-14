//! Per-provider health tracking, so a broken route stops absorbing requests.
//!
//! Today [`crate::providers::RouteRegistry::resolve`] hands back whatever route it was asked for,
//! and nothing observes that the route has failed the last twenty times: the caller asks again, the
//! provider fails again, and every request pays the full timeout before it learns what the previous
//! twenty already knew. Retrying does not close that gap — [`crate::retrying_provider`] retries the
//! *same* provider, so it re-spends the same failure instead of finding a working one.
//!
//! This module supplies the missing memory: consecutive failures per provider, and a cooldown during
//! which that provider is not picked. It is deliberately only the bookkeeping — deciding *which*
//! failures to record belongs to the caller that saw the error, and deciding *what to do* when
//! nothing is available belongs to the caller that owns the request.
//!
//! # Why a threshold rather than trip-on-first-failure
//!
//! One failure is not evidence of a broken provider; it is also what a single dropped connection
//! looks like. Tripping on the first error converts a blip into an outage: the provider is pulled
//! from rotation exactly when the traffic it was serving has to go somewhere, and a merely flaky
//! provider loses every request it would have answered. The threshold is therefore a parameter, and
//! the count is *consecutive*: a success in between means the provider recovered, so the count
//! starts again.
//!
//! # Why the cooldown ends in `Degraded`, not `Healthy`
//!
//! A cooldown says "the evidence is stale", not "the fault is fixed". Reporting [`Health::Healthy`]
//! at expiry would put a provider that is still broken straight back into full rotation, where it
//! would absorb the whole cooldown's worth of traffic before the threshold tripped again.
//! [`Health::Degraded`] says what is actually true: attempt it, but do not assume it works. The
//! first success clears the count; the first failure re-opens the cooldown, because a provider that
//! failed its probe is the same provider that failed before.
//!
//! # Why selection is ordered
//!
//! [`HealthTracker::pick`] walks the caller's list in order. A caller builds that list from its own
//! preference — configured priority, cost, latency — and a tracker that rotated or randomised would
//! silently override a decision it cannot see.

// The crate root is wired up separately, so until then nothing in the library references these
// items. The allow is scoped to this module and disappears in effect once it is exported.
#![allow(dead_code)]

use std::collections::HashMap;

/// A provider's health, as of one instant.
///
/// The failure count is carried in `Degraded` because it is the evidence behind the state: a caller
/// that logs "degraded after 3 failures" has something to act on, while one that logs "degraded"
/// has to go and ask.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Health {
    /// Working, or never failed. Nothing is being held back.
    Healthy,
    /// Failed recently, but still worth attempting.
    Degraded {
        /// How many failures have accumulated since the last success.
        failures: u32,
    },
    /// Should not be used until the cooldown ends.
    Unavailable {
        /// Wall-clock milliseconds, in the caller's own clock, when attempting becomes allowed
        /// again. Carried so a caller can report a retry-after without re-reading the tracker.
        until_ms: u64,
        /// The last failure that tripped the threshold, for the log line or the operator.
        reason: String,
    },
}

/// One provider's accumulated history.
///
/// A missing map entry means the same thing as `Entry::default()`: no failures, no cooldown. The
/// tracker stores nothing on success for that reason — see [`HealthTracker::record_success`].
#[derive(Clone, Debug, Default)]
struct Entry {
    /// Failures since the last success. Zero means the provider has not failed since then.
    consecutive_failures: u32,
    /// When set, the instant the cooldown expires. `None` means no cooldown is in force.
    unavailable_until_ms: Option<u64>,
    /// The most recent failure's reason. Kept after the cooldown ends, so a later report can still
    /// say why this provider was doubted.
    reason: String,
}

/// Tracks per-provider health and picks a provider that may be attempted.
///
/// The tracker holds no clock: every method that needs the time takes it as an argument. That keeps
/// the cooldown arithmetic testable without sleeping, and it keeps one shared tracker usable by
/// callers whose clocks are not the ones the tracker was constructed with.
#[derive(Clone, Debug)]
pub struct HealthTracker {
    /// Consecutive failures that trip the cooldown. Clamped to at least one.
    threshold: u32,
    /// How long a tripped provider is held out, in milliseconds.
    cooldown_ms: u64,
    /// One entry per provider that has failed. Healthy providers have no entry.
    entries: HashMap<String, Entry>,
}

impl HealthTracker {
    /// Creates a tracker.
    ///
    /// `threshold` is clamped to at least one. A threshold of zero would mark every provider
    /// unavailable before it had done anything, which is a misconfiguration that should not be able
    /// to produce a tracker that refuses the entire rotation.
    #[must_use]
    pub fn new(threshold: u32, cooldown_ms: u64) -> Self {
        Self {
            threshold: threshold.max(1),
            cooldown_ms,
            entries: HashMap::new(),
        }
    }

    /// The failure count that trips the cooldown, after clamping.
    #[must_use]
    pub fn threshold(&self) -> u32 {
        self.threshold
    }

    /// The cooldown applied when the threshold is reached, in milliseconds.
    #[must_use]
    pub fn cooldown_ms(&self) -> u64 {
        self.cooldown_ms
    }

    /// Records that `provider` answered.
    ///
    /// The entry is removed rather than zeroed. A success means there is nothing left to know about
    /// this provider, and removal gives "healthy" exactly one representation — the absence of
    /// history — instead of two that can drift apart. It also stops a recovered provider from
    /// occupying memory in a tracker that may be long-lived.
    pub fn record_success(&mut self, provider: &str) {
        self.entries.remove(provider);
    }

    /// Records that `provider` failed at `now_ms`.
    ///
    /// Only a *consecutive* run of failures trips the cooldown: the count is cleared by
    /// [`Self::record_success`], so a provider that fails, recovers, and fails again is back at one
    /// failure rather than two.
    ///
    /// A failure that arrives while the provider is still cooling down re-opens the cooldown from
    /// the new failure's instant. A caller is not supposed to attempt a provider the tracker
    /// refuses, but if one does and it fails again, the honest reading is that the cooldown had not
    /// expired for a reason.
    pub fn record_failure(&mut self, provider: &str, now_ms: u64, reason: &str) {
        let entry = self.entries.entry(provider.to_owned()).or_default();
        entry.consecutive_failures = entry.consecutive_failures.saturating_add(1);
        // `clone_into` rather than assignment: the slot is already allocated, so a reallocation per
        // failure would be paid on every request that fails rather than once per provider.
        reason.clone_into(&mut entry.reason);
        if entry.consecutive_failures >= self.threshold {
            // `saturating_add` rather than `+`: an extreme clock reading must not panic, and a
            // cooldown that saturates is a cooldown that lasts, which fails safe.
            entry.unavailable_until_ms = Some(now_ms.saturating_add(self.cooldown_ms));
        }
    }

    /// Reports `provider`'s health at `now_ms`.
    ///
    /// The cooldown is evaluated here rather than cleared by a timer: there is no background task to
    /// run, and a lazily expired cooldown gives the same answer to a caller that asks without
    /// anything having to be scheduled. Expiry yields [`Health::Degraded`] and leaves the failure
    /// count in place — see the module docs for why it is not `Healthy`.
    #[must_use]
    pub fn health(&self, provider: &str, now_ms: u64) -> Health {
        let Some(entry) = self.entries.get(provider) else {
            return Health::Healthy;
        };
        if let Some(until_ms) = entry.unavailable_until_ms {
            if now_ms < until_ms {
                return Health::Unavailable {
                    until_ms,
                    reason: entry.reason.clone(),
                };
            }
        }
        // Reached when there is no cooldown, or when the cooldown has just expired. A zero count
        // here would mean an entry survived a success, which `record_success` prevents; the branch
        // is still written so that such an entry reports `Healthy` rather than `Degraded { 0 }`.
        if entry.consecutive_failures == 0 {
            return Health::Healthy;
        }
        Health::Degraded {
            failures: entry.consecutive_failures,
        }
    }

    /// Picks the first provider in `providers` that may be attempted at `now_ms`.
    ///
    /// Order is the caller's preference and is preserved exactly: this walks the slice from the
    /// front and returns the first entry that is not [`Health::Unavailable`]. No rotation or
    /// randomisation, because either would override a priority the tracker cannot see.
    ///
    /// A [`Health::Degraded`] provider is still picked. Degraded means "failed recently", and a
    /// caller with no healthier alternative is better off trying the provider that answered before
    /// than failing outright on one it never tried.
    ///
    /// Returns `None` when every provider is cooling down. What that means — a queue, a fallback
    /// outside this list, or an error — is the caller's decision, not this one's.
    #[must_use]
    pub fn pick<'a>(&self, providers: &'a [String], now_ms: u64) -> Option<&'a String> {
        providers
            .iter()
            .find(|provider| !matches!(self.health(provider, now_ms), Health::Unavailable { .. }))
    }
}
