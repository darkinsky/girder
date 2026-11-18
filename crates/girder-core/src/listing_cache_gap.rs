//! `ModelListingCache`: the credential-scoped `/models` cache as a pure state machine.
//!
//! Upstream path: `packages/llm/llm-deepseek/src/listing-cache.ts` (202 lines), with its suite at
//! `packages/llm/llm-deepseek/tests/listing-cache.spec.ts` (416 lines, 21 cases).
//!
//! Allowing `doc_markdown`: this file names `DeepSeek`, `Messages`, `GET`, and `/models` as API
//! nouns in nearly every paragraph, and quoting each one would make the prose about punctuation
//! instead of about behaviour. The peer `deepseek_*_gap` modules do the same.
#![allow(
    clippy::doc_markdown,
    reason = "provider and endpoint names are the subject of nearly every paragraph"
)]
// The crate root does not declare this module; the integrator owns that line. Until then nothing in
// a library build has a caller, so the whole surface would read as waste.
#![allow(
    dead_code,
    reason = "the crate root's `pub mod` line belongs to the integrator; the test target exercises every item"
)]

use std::fmt;

use serde_json::{Map, Value};

use girder_core::deepseek_options_gap::ListingSettings;
use girder_core::llm_extra_gap::MAX_TIMER_DELAY_MS;
use girder_core::persistence_win32_gap::sha256_hex;

/// The timeout reason a listing deadline reports — upstream `LISTING_TIMEOUT`.
pub const LISTING_TIMEOUT: &str = "MODEL_LISTING_TIMEOUT";

/// The `LlmError` code every extension-preparation refusal carries — upstream `REQUEST_EXTENSION`.
pub const REQUEST_EXTENSION: &str = "REQUEST_EXTENSION";

/// What one decoded listing has to say about itself — upstream's `entries.length` and `skipped`.
///
/// The cache reports both in its one info line, so it needs them from any listing type rather than
/// only from [`girder_core::deepseek_files_gap::ModelListing`].
pub trait ListingFacts {
    /// How many usable entries the listing carries.
    fn entries(&self) -> usize;
    /// How many rows were dropped, for a missing or repeated id.
    fn skipped(&self) -> usize;
}

impl ListingFacts for girder_core::deepseek_files_gap::ModelListing {
    fn entries(&self) -> usize {
        self.entries.len()
    }

    fn skipped(&self) -> usize {
        self.skipped
    }
}

/// What a reader gets — upstream `ListingView`.
///
/// `authenticated` is decided by whether the route has a credential at all, not by whether a load
/// has succeeded: a caller must be able to tell "no credential, show the bundled catalog" from
/// "credential present, listing not loaded yet".
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ListingView<'a, L> {
    /// Whether a credential is available for the listing endpoint.
    pub authenticated: bool,
    /// Last successful listing for this scope; `None` until one succeeds.
    pub listing: Option<&'a L>,
}

/// One load the driver must perform — the state machine's half of `start()`.
///
/// A `Load` always cancels the pending refresh timer: upstream's `start` calls `clearTimeout` before
/// installing the request, including on the single-flight path where it returns early. The driver
/// therefore clears its timer whenever a call yields a `Load`, and only then.
#[derive(Clone, Debug, PartialEq)]
pub struct Load {
    /// Generation this load belongs to. Hand it back to [`ListingCacheState::settle`].
    pub generation: u64,
    /// Endpoint the load reads, kept for the diagnostic the settlement formats.
    pub url: String,
    /// Deadline the driver must impose on the request, in milliseconds.
    pub timeout_ms: f64,
}

/// What one access resolves to — the state machine's half of `view()`.
#[derive(Clone, Debug, PartialEq)]
pub enum Access {
    /// The cache was disposed; access changes nothing and returns the retained view.
    Closed,
    /// No credential for this endpoint, so the scope is unauthenticated and nothing loads.
    Unauthenticated,
    /// The first access of a scope: the caller waits for this load, which the deadline bounds.
    ///
    /// `None` means another caller already started it, and this caller waits on that same load
    /// (single-flight). The wait is *abandonable*: a caller that gives up stops awaiting and the
    /// shared load keeps running, so a later caller still gets its result.
    Wait { load: Option<Load> },
    /// A later access of the scope: it returns at once and refreshes in the background.
    ///
    /// `Some` means the listing is at least `max_age_ms` old (or `max_age_ms` is zero, which
    /// refreshes on every access); the driver starts the load without awaiting it.
    Immediate { refresh: Option<Load> },
}

/// What one settlement did — the state machine's half of `start()`'s two handlers and `finally`.
///
/// `published` distinguishes "this load's outcome was applied" from "a rescope, an invalidation, or
/// a disposal happened first, so the result is discarded". A discarded result is silent: no info
/// line, no warning, no notification, no reschedule.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Settlement {
    /// Whether the outcome was applied to the cache.
    pub published: bool,
    /// Whether the published listing differs from the previous one.
    pub changed: bool,
    /// The info line a published success reports, when it is worth reporting.
    pub info: Option<String>,
    /// The warning a first failure of a streak reports.
    pub warning: Option<String>,
    /// Whether a changed view must be published to observers.
    pub notify: bool,
    /// The delay before the next automatic refresh, when one is scheduled.
    pub refresh_in: Option<u64>,
}

impl Settlement {
    /// The settlement of a load whose generation was superseded.
    #[must_use]
    pub fn discarded() -> Self {
        Self::default()
    }
}

/// What the refresh timer should do — the state machine's half of `refresh()`'s first half.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RefreshStart {
    /// The cache was disposed; do nothing and do not reschedule.
    Closed,
    /// The route's settings vanished, so the listing is dropped and the change is published.
    Disabled,
    /// Resolve the credential and call [`ListingCacheState::access`]; a following
    /// [`ListingCacheState::note_refresh_view`] publishes a dropped authentication.
    Continue {
        /// Whether the route was authenticated before this refresh.
        was_authenticated: bool,
    },
}

/// The credential-scoped listing cache, without I/O, time, or tasks.
///
/// # The four rules this holds
///
/// 1. **Scope invalidation.** A scope is the endpoint plus a digest of the credential — the pair,
///    not the credential alone, because one token may be valid at two origins and the listing at
///    one says nothing about the other. A scope change discards the listing, the settle time, the
///    in-flight load, and the failing streak, and bumps the generation.
/// 2. **Single flight.** While a load is in flight, `start` yields nothing, so concurrent callers
///    join the one load instead of multiplying requests.
/// 3. **Generation fencing.** Every start, rescope, invalidation, and disposal bumps a counter. A
///    load carries the counter it started under, and a settlement whose counter has moved on is
///    discarded. This is what stops a slow load from publishing over a newer one, and what makes
///    disposal final without awaiting anything.
/// 4. **Last-known-good retention.** A failure sets the settle time (so the retry keeps the same
///    interval) and never clears the listing; badges and all survive a failing streak.
///
/// # What it deliberately does not do
///
/// No HTTP, no `AbortController`, no `setTimeout`, no clock, no `credential()` lookup. Those are the
/// driver's, and [`girder_core::deepseek_files_gap::parse_model_listing`] is the decoder on the
/// other side of the fetch. Every decision above is decidable from arguments, which is why the
/// upstream suite's 21 cases can be transcribed here without a server, a timer, or a task.
#[derive(Clone, Debug)]
pub struct ListingCacheState<L> {
    /// Endpoint plus credential digest, or `None` while unauthenticated.
    scope: Option<String>,
    listing: Option<L>,
    /// Settle time of the latest load in this scope; `None` before the first one settles.
    settled_at: Option<u64>,
    /// Generation of the load in flight, when one is.
    inflight: Option<u64>,
    generation: u64,
    failing: bool,
    closed: bool,
}

impl<L> Default for ListingCacheState<L> {
    fn default() -> Self {
        Self {
            scope: None,
            listing: None,
            settled_at: None,
            inflight: None,
            generation: 0,
            failing: false,
            closed: false,
        }
    }
}

impl<L> ListingCacheState<L> {
    /// A cache with no scope, no listing, and no in-flight load.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// The last published view, without I/O — upstream `current()`.
    #[must_use]
    pub fn current(&self) -> ListingView<'_, L> {
        ListingView {
            authenticated: self.scope.is_some(),
            listing: self.listing.as_ref(),
        }
    }

    /// The scope this cache holds, exposed for diagnostics and for tests.
    #[must_use]
    pub fn scope(&self) -> Option<&str> {
        self.scope.as_deref()
    }

    /// Whether [`ListingCacheState::dispose`] has run.
    #[must_use]
    pub fn is_closed(&self) -> bool {
        self.closed
    }

    /// Whether the current streak of loads is failing.
    #[must_use]
    pub fn is_failing(&self) -> bool {
        self.failing
    }

    /// When the latest load settled, if one has.
    #[must_use]
    pub fn settled_at(&self) -> Option<u64> {
        self.settled_at
    }

    /// Whether a load is in flight.
    #[must_use]
    pub fn is_inflight(&self) -> bool {
        self.inflight.is_some()
    }

    /// Resolve one access — upstream `view()` after the credential lookup.
    ///
    /// `credential` is the resolved credential for `settings.url`; resolving it is the driver's
    /// job, and so is refusing an already-aborted caller *after* that resolution, which is the order
    /// upstream performs. `now` is the wall clock in milliseconds, used only for the staleness
    /// comparison.
    pub fn access(
        &mut self,
        settings: &ListingSettings,
        credential: Option<&str>,
        now: u64,
    ) -> Access {
        if self.closed {
            return Access::Closed;
        }
        let scope = scope_of(&settings.url, credential);
        if scope != self.scope {
            self.rescope(scope);
        }
        if credential.is_none() {
            return Access::Unauthenticated;
        }
        match self.settled_at {
            None => Access::Wait {
                load: self.start(settings),
            },
            Some(settled_at) if now.saturating_sub(settled_at) >= settings.max_age_ms => {
                Access::Immediate {
                    refresh: self.start(settings),
                }
            }
            Some(_) => Access::Immediate { refresh: None },
        }
    }

    /// Apply one load's outcome — upstream's `then`/`catch`/`finally` around `load`.
    ///
    /// The settlement always clears the in-flight slot when it still names `generation`, even when
    /// the result itself is discarded, because that slot is the single-flight latch: leaving it set
    /// would deadlock every later access.
    pub fn settle(
        &mut self,
        generation: u64,
        outcome: Result<L, String>,
        url: &str,
        settings: &ListingSettings,
        now: u64,
    ) -> Settlement
    where
        L: PartialEq + ListingFacts,
    {
        if self.inflight == Some(generation) {
            self.inflight = None;
        }
        if generation != self.generation {
            return Settlement::discarded();
        }
        match outcome {
            Ok(listing) => {
                let changed = self.listing.as_ref() != Some(&listing);
                let info = info_message(url, listing.entries(), listing.skipped());
                self.listing = Some(listing);
                self.settled_at = Some(now);
                self.failing = false;
                Settlement {
                    published: true,
                    changed,
                    info: Some(info),
                    warning: None,
                    notify: changed,
                    refresh_in: self.refresh_delay(settings, now),
                }
            }
            Err(error) => {
                let warning =
                    (!self.failing).then(|| failure_warning(url, &error, self.listing.is_some()));
                self.settled_at = Some(now);
                self.failing = true;
                Settlement {
                    published: true,
                    changed: false,
                    info: None,
                    warning,
                    notify: false,
                    refresh_in: self.refresh_delay(settings, now),
                }
            }
        }
    }

    /// Discard the listing and abandon the in-flight load — upstream `invalidate()`.
    ///
    /// The scope is *kept*, so the view still reports the route as authenticated while carrying no
    /// listing. The next access therefore waits for a fresh load, exactly as a first access does.
    ///
    /// Returns whether a request was in flight and must be aborted.
    pub fn invalidate(&mut self) -> bool {
        let scope = self.scope.clone();
        self.rescope(scope)
    }

    /// Stop refreshing and fence off every later result — upstream `dispose()`.
    ///
    /// Awaited loads are the driver's business (upstream `Promise.allSettled(this.pending)`); this
    /// decides only that the cache is closed and that any settlement still to arrive is discarded.
    ///
    /// Returns whether a request is in flight and must be aborted.
    pub fn dispose(&mut self) -> bool {
        let aborted = self.inflight.take().is_some();
        self.closed = true;
        self.generation += 1;
        aborted
    }

    /// The delay before the next automatic refresh, or `None` when there is none.
    ///
    /// `max_age_ms == 0` disables the timer on purpose: zero means "refresh on each access", and a
    /// timer at zero would spin. The delay is floored at 1 ms so a listing that settled a millisecond
    /// ago does not schedule an immediate second load, and capped at [`MAX_TIMER_DELAY_MS`] so a
    /// route configured with a week cannot overflow a runtime's timer.
    #[must_use]
    pub fn refresh_delay(&self, settings: &ListingSettings, now: u64) -> Option<u64> {
        if self.closed || settings.max_age_ms == 0 {
            return None;
        }
        let age = self
            .settled_at
            .map_or(0, |settled_at| now.saturating_sub(settled_at));
        let remaining = settings.max_age_ms.saturating_sub(age);
        Some(remaining.clamp(1, MAX_TIMER_DELAY_MS))
    }

    /// What the refresh timer does when it fires — upstream `refresh()`'s first half.
    ///
    /// `settings_supplied` is whether [`girder_core::deepseek_options_gap::ListingSettings`] still
    /// resolves: a route whose listing was switched off (or whose file was removed) stops being
    /// followed, and its listing is dropped rather than kept as a stale one.
    pub fn begin_refresh(&mut self, settings_supplied: bool) -> RefreshStart {
        if self.closed {
            return RefreshStart::Closed;
        }
        if settings_supplied {
            RefreshStart::Continue {
                was_authenticated: self.scope.is_some(),
            }
        } else {
            self.invalidate();
            RefreshStart::Disabled
        }
    }

    /// Whether a refresh must publish that the route lost its credential — upstream's
    /// `if (wasAuthenticated && !current().authenticated) notify()`.
    ///
    /// Only the true→false edge is reported: a route that was already unauthenticated stays quiet on
    /// every following refresh, and a route that gains a credential reports through the load that
    /// then succeeds.
    pub fn note_refresh_view(&mut self, was_authenticated: bool) -> bool {
        was_authenticated && !self.current().authenticated
    }

    /// Apply a refresh that threw before it could reach a settlement — upstream's `catch`.
    ///
    /// The usual cause is the credential lookup failing, which never reaches [`Self::settle`], so
    /// this is the only path that can report it. Returns the schedule the timer must install, or
    /// `None` when a load is already in flight and will reschedule when it settles.
    ///
    /// # Errors-free by construction
    ///
    /// The warning is returned rather than logged, because upstream logs through the host's logger
    /// and this module owns no logger; the driver reports it verbatim.
    pub fn refresh_failed(
        &mut self,
        error: &str,
        settings: &ListingSettings,
        now: u64,
    ) -> RefreshFailure {
        if self.closed {
            return RefreshFailure::default();
        }
        let warning = (!self.failing).then(|| refresh_failure_warning(error));
        self.failing = true;
        self.settled_at = Some(now);
        RefreshFailure {
            warning,
            refresh_in: if self.inflight.is_none() {
                self.refresh_delay(settings, now)
            } else {
                None
            },
        }
    }

    /// The schedule a completed refresh installs — upstream's final
    /// `if (this.inflight === undefined) this.schedule(settings)`.
    #[must_use]
    pub fn refresh_completed(&self, settings: &ListingSettings, now: u64) -> Option<u64> {
        if self.inflight.is_some() {
            return None;
        }
        self.refresh_delay(settings, now)
    }

    /// The single-flight latch, and the generation fence — upstream `start()`'s first half.
    fn start(&mut self, settings: &ListingSettings) -> Option<Load> {
        if self.inflight.is_some() {
            return None;
        }
        self.generation += 1;
        let load = Load {
            generation: self.generation,
            url: settings.url.clone(),
            timeout_ms: settings.timeout_ms,
        };
        self.inflight = Some(self.generation);
        Some(load)
    }

    /// Drop everything belonging to the previous scope — upstream `rescope()`.
    ///
    /// Returns whether a request was in flight and must be aborted. The generation moves even when
    /// the scope does not (invalidation rescopes to the same scope), which is what discards the
    /// result of a load that was already running.
    fn rescope(&mut self, scope: Option<String>) -> bool {
        let aborted = self.inflight.take().is_some();
        self.scope = scope;
        self.listing = None;
        self.settled_at = None;
        self.failing = false;
        self.generation += 1;
        aborted
    }
}

/// A refresh that failed before reaching a settlement — upstream `refresh()`'s `catch`.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct RefreshFailure {
    /// The warning a first failure of a streak reports.
    pub warning: Option<String>,
    /// The delay the timer must install.
    pub refresh_in: Option<u64>,
}

/// The scope of one endpoint and credential — upstream's
/// `` `${settings.url}\n${createHash('sha256').update(credential).digest('hex')}` ``.
///
/// The digest rather than the credential is what makes a scope safe to log and to keep in a struct
/// that outlives the credential: a plaintext token in a diagnostic is a leak, and the scope string
/// is the kind of value that reaches one. Two tokens hash to two scopes even at the same endpoint,
/// which is what forces a reload when a credential is rotated.
#[must_use]
pub fn scope_of(url: &str, credential: Option<&str>) -> Option<String> {
    credential.map(|credential| format!("{url}\n{}", sha256_hex(credential.as_bytes())))
}

/// The info line a successful load reports — upstream's `info(...)` format string.
#[must_use]
pub fn info_message(url: &str, entries: usize, skipped: usize) -> String {
    format!("model listing from {url} loaded {entries} model(s), skipped {skipped} row(s)")
}

/// The warning the *first* failure of a streak reports — upstream's `warn(...)` format string.
///
/// Which fallback is named depends on whether there is a listing to keep: a route that never loaded
/// falls back to the bundled catalog, and one that loaded before keeps the previous catalog,
/// badges and all.
#[must_use]
pub fn failure_warning(url: &str, error: &str, has_previous: bool) -> String {
    let fallback = if has_previous {
        "keeping the previous catalog"
    } else {
        "using the bundled catalog"
    };
    format!("model listing from {url} failed ({error}); {fallback}")
}

/// The warning a refresh that threw reports — upstream's `refresh failed (...)` format string.
#[must_use]
pub fn refresh_failure_warning(error: &str) -> String {
    format!(
        "model listing refresh failed ({error}); keeping the previous catalog; check listing \
         configuration and credentials"
    )
}

/// The warning a failing change observer reports — upstream's `notify()` catch.
///
/// The listing stays committed: reporting a change and publishing one are different jobs, and a
/// broken observer must not cost the caller the catalog it just loaded.
#[must_use]
pub fn notification_failure_warning(error: &str) -> String {
    format!("model listing change notification failed ({error}); reload the model catalog")
}

// =============================================================================================
// `prepareRequestExtensions`: the `REQUEST_EXTENSION` collision rule
// =============================================================================================
//
// Upstream path: `packages/llm/llm-deepseek/src/request-extensions.ts` (59 lines). Most of it is
// transport: it awaits a contributor registry, serializes, and commits acceptance after HTTP 2xx.
// Two decisions in it are pure and were both absent from this crate:
//
// 1. **A contributed field may not shadow a base request field.** `Object.hasOwn(body, field)`
//    decides it, so the test is membership of the *base request object*, not of any nested value:
//    an extension may add `metadata` and may not add `messages`.
// 2. **A base request that cannot serialize yields the base alone**, with the contributed fields
//    reported as omitted so their owners resend unaccepted state on the next request.
//
// (2) has no Rust counterpart — `serde_json::Value` cannot be cyclic, so serialization does not
// fail — and is reported rather than emulated. (1) is [`merge_request_extensions`].

/// Why extension fields could not be merged into a base request.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum RequestExtensionError {
    /// A contributed field is already a member of the base request.
    Collision {
        /// The field name, as the diagnostic quotes it.
        field: String,
    },
    /// The base request was not a JSON object, so it has no field set to compare against.
    NotAnObject,
}

impl fmt::Display for RequestExtensionError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Collision { field } => formatter.write_str(&format!(
                "DeepSeek request extension field {} collides with the base request",
                quote_json(field)
            )),
            Self::NotAnObject => formatter.write_str(
                "DeepSeek request extension preparation failed: the base request is not a JSON object",
            ),
        }
    }
}

impl std::error::Error for RequestExtensionError {}

/// The code every refusal above carries — [`REQUEST_EXTENSION`].
impl RequestExtensionError {
    /// The `LlmError` code this refusal is raised under.
    ///
    /// An associated function rather than a method, because the code is a property of the variant
    /// set and not of one value: every refusal this module raises carries [`REQUEST_EXTENSION`], so
    /// a method taking `self` would imply a distinction that does not exist.
    #[must_use]
    pub const fn code() -> &'static str {
        REQUEST_EXTENSION
    }
}

/// Merge contributed fields over a base Messages request — upstream
/// `{ ...body, ...extensions.fields }`, preceded by the collision check.
///
/// The base request is cloned, so a caller that keeps its body keeps it intact; the contributed
/// fields are inserted after the check, which is why a collision can never silently win.
///
/// # Errors
///
/// [`RequestExtensionError::Collision`] when a contributed field is already a member of the base
/// request — upstream refuses rather than overwriting, because a plugin and the serializer
/// disagreeing about `messages` is a bug a merge would hide — and
/// [`RequestExtensionError::NotAnObject`] when `base` is not a JSON object.
///
/// # Determinism
///
/// Which of several colliding fields is named depends on the map's iteration order: upstream reads
/// `Object.keys(extensions.fields)` (insertion order) and `serde_json::Map` is a `BTreeMap` unless
/// the `preserve_order` feature is on, so this build reports the smallest name first. A caller that
/// needs upstream's exact choice must check [`colliding_field`] itself, one field at a time.
pub fn merge_request_extensions(
    base: &Value,
    fields: &Map<String, Value>,
) -> Result<Value, RequestExtensionError> {
    let Value::Object(body) = base else {
        return Err(RequestExtensionError::NotAnObject);
    };
    if let Some(field) = colliding_field(body, fields) {
        return Err(RequestExtensionError::Collision {
            field: field.to_owned(),
        });
    }
    let mut merged = body.clone();
    for (field, value) in fields {
        merged.insert(field.clone(), value.clone());
    }
    Ok(Value::Object(merged))
}

/// The first contributed field that the base request already carries, if any.
///
/// This is upstream's `Object.hasOwn(body, field)` per field, in one pass. A field whose base value
/// is `null` still collides: presence is the test, not truthiness.
#[must_use]
pub fn colliding_field<'a>(
    base: &Map<String, Value>,
    fields: &'a Map<String, Value>,
) -> Option<&'a str> {
    fields
        .keys()
        .find(|field| base.contains_key(field.as_str()))
        .map(String::as_str)
}

/// `JSON.stringify` for a string, as the collision message spells it.
///
/// A field name is quoted rather than interpolated raw so a name carrying a quote or a newline
/// cannot make the diagnostic ambiguous.
fn quote_json(text: &str) -> String {
    Value::String(text.to_owned()).to_string()
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;
    use girder_core::deepseek_options_gap::DEFAULT_MODELS_MAX_AGE_MS;

    fn settings(max_age_ms: u64) -> ListingSettings {
        ListingSettings {
            url: "https://api.example/models".to_owned(),
            timeout_ms: 5_000.0,
            max_age_ms,
        }
    }

    fn cache() -> ListingCacheState<girder_core::deepseek_files_gap::ModelListing> {
        ListingCacheState::new()
    }

    #[test]
    fn the_default_interval_is_the_upstream_one() {
        assert_eq!(DEFAULT_MODELS_MAX_AGE_MS, 600_000);
    }

    #[test]
    fn a_scope_digests_the_credential() {
        let scope = scope_of("https://api.example/models", Some("token-a")).expect("a scope");
        assert_eq!(
            scope,
            "https://api.example/models\na70bf50e531ce1a817561f2f5d5b6645d4e806becf58ccc5e8cf6b8045a090a8"
        );
        assert_eq!(scope_of("https://api.example/models", None), None);
        assert_ne!(
            scope_of("https://api.example/models", Some("token-a")),
            scope_of("https://api.example/models", Some("token-b"))
        );
        assert_ne!(
            scope_of("https://api.example/models", Some("token-a")),
            scope_of("https://other.example/models", Some("token-a"))
        );
    }

    #[test]
    fn an_unauthenticated_access_loads_nothing_and_reports_no_scope() {
        let mut state = cache();
        assert_eq!(
            state.access(&settings(100), None, 0),
            Access::Unauthenticated
        );
        assert_eq!(
            state.current(),
            ListingView {
                authenticated: false,
                listing: None
            }
        );
        assert!(!state.is_inflight());
    }

    #[test]
    fn the_first_access_starts_one_load_that_concurrent_callers_join() {
        let mut state = cache();
        let first = state.access(&settings(100), Some("token-a"), 0);
        let Access::Wait { load: Some(load) } = first else {
            panic!("the first access must wait for a fresh load");
        };
        // One rescope and one start: upstream bumps the counter in both places.
        assert_eq!(load.generation, 2);
        assert_eq!(load.url, "https://api.example/models");
        assert!((load.timeout_ms - 5_000.0).abs() < f64::EPSILON);
        // A concurrent caller joins the same load rather than starting a second one.
        assert_eq!(
            state.access(&settings(100), Some("token-a"), 0),
            Access::Wait { load: None }
        );
        assert!(state.is_inflight());
    }

    #[test]
    fn a_stale_listing_refreshes_on_access_and_a_fresh_one_does_not() {
        let mut state = cache();
        let Access::Wait { load: Some(load) } = state.access(&settings(100), Some("token-a"), 0)
        else {
            panic!("a fresh scope waits");
        };
        // A settlement is applied and schedules the next refresh.
        let settlement = state.settle(
            load.generation,
            Err("down".to_owned()),
            &load.url,
            &settings(100),
            0,
        );
        assert!(settlement.published);
        assert_eq!(settlement.refresh_in, Some(100));
        // Just short of the interval: nothing refreshes.
        assert_eq!(
            state.access(&settings(100), Some("token-a"), 99),
            Access::Immediate { refresh: None }
        );
        // At the interval: a background refresh starts.
        let Access::Immediate {
            refresh: Some(refresh),
        } = state.access(&settings(100), Some("token-a"), 100)
        else {
            panic!("a stale listing refreshes");
        };
        assert_eq!(refresh.generation, 3);
    }

    #[test]
    fn the_delay_is_floored_at_one_and_capped_at_the_timer_ceiling() {
        let mut state = cache();
        let Access::Wait { load: Some(load) } = state.access(&settings(0), Some("token-a"), 0)
        else {
            panic!("a fresh scope waits");
        };
        let _ = state.settle(
            load.generation,
            Err("down".to_owned()),
            &load.url,
            &settings(0),
            0,
        );
        assert_eq!(
            state.refresh_delay(&settings(0), 0),
            None,
            "zero disables the timer"
        );
        assert_eq!(state.refresh_delay(&settings(1), 0), Some(1));
        assert_eq!(
            state.refresh_delay(&settings(u64::MAX), 0),
            Some(MAX_TIMER_DELAY_MS)
        );
        assert_eq!(state.refresh_delay(&settings(100), 60), Some(40));
        assert_eq!(state.refresh_delay(&settings(100), 5_000), Some(1));
    }

    #[test]
    fn a_rescope_on_another_credential_discards_the_old_result() {
        let mut state = cache();
        let Access::Wait { load: Some(old) } = state.access(&settings(100), Some("token-a"), 0)
        else {
            panic!("a fresh scope waits");
        };
        let Access::Wait { load: Some(new) } = state.access(&settings(100), Some("token-b"), 0)
        else {
            panic!("another credential is another scope");
        };
        assert_ne!(old.generation, new.generation);
        // The superseded load settles late and is discarded.
        let late = state.settle(
            old.generation,
            Err("old".to_owned()),
            &old.url,
            &settings(100),
            5,
        );
        assert_eq!(late, Settlement::discarded());
        // Its generation no longer names the in-flight load, so the new one survives.
        assert!(state.is_inflight());
    }

    #[test]
    fn dropping_the_credential_unauthorises_the_scope() {
        let mut state = cache();
        let _ = state.access(&settings(100), Some("token-a"), 0);
        assert_eq!(
            state.access(&settings(100), None, 10),
            Access::Unauthenticated
        );
        assert!(!state.current().authenticated);
        assert_eq!(state.current().listing, None);
    }

    #[test]
    fn an_invalidation_keeps_the_scope_and_the_next_access_waits_again() {
        let mut state = cache();
        let _ = state.access(&settings(100), Some("token-a"), 0);
        let aborted = state.invalidate();
        assert!(aborted, "the in-flight load is abandoned");
        assert!(state.current().authenticated);
        assert_eq!(state.current().listing, None);
        assert!(!state.is_inflight());
        let Access::Wait { load: Some(_) } = state.access(&settings(100), Some("token-a"), 0)
        else {
            panic!("an invalidated scope waits like a first access");
        };
    }

    #[test]
    fn a_disposal_is_final() {
        let mut state = cache();
        let _ = state.access(&settings(100), Some("token-a"), 0);
        assert!(state.dispose());
        assert!(state.is_closed());
        assert_eq!(
            state.access(&settings(100), Some("token-a"), 0),
            Access::Closed
        );
        assert_eq!(state.refresh_delay(&settings(100), 0), None);
        assert_eq!(state.begin_refresh(true), RefreshStart::Closed);
        assert_eq!(
            state.refresh_failed("late", &settings(100), 0),
            RefreshFailure::default()
        );
    }

    #[test]
    fn a_refresh_prefers_live_settings_and_stops_following_a_disabled_route() {
        let mut state = cache();
        let _ = state.access(&settings(100), Some("token-a"), 0);
        assert_eq!(
            state.begin_refresh(true),
            RefreshStart::Continue {
                was_authenticated: true
            }
        );
        assert_eq!(state.begin_refresh(false), RefreshStart::Disabled);
        assert_eq!(state.current().listing, None);
        assert!(state.current().authenticated);
    }

    #[test]
    fn losing_the_credential_on_a_refresh_is_reported_once() {
        let mut state = cache();
        let _ = state.access(&settings(100), Some("token-a"), 0);
        let RefreshStart::Continue { was_authenticated } = state.begin_refresh(true) else {
            panic!("settings are still supplied");
        };
        assert!(was_authenticated);
        let _ = state.access(&settings(100), None, 10);
        assert!(state.note_refresh_view(was_authenticated));
    }

    #[test]
    fn a_refresh_that_threw_warns_once_and_schedules_its_retry() {
        let mut state = cache();
        let Access::Wait { load: Some(load) } = state.access(&settings(100), Some("token-a"), 0)
        else {
            panic!("a fresh scope waits");
        };
        // The streak has not warned yet: the load succeeded, so a lookup failure is the first news.
        let _ = state.settle(
            load.generation,
            Ok(girder_core::deepseek_files_gap::ModelListing {
                entries: Vec::new(),
                skipped: 0,
            }),
            &load.url,
            &settings(100),
            0,
        );
        let first = state.refresh_failed("credentials offline", &settings(100), 100);
        assert_eq!(
            first.warning.as_deref(),
            Some(
                "model listing refresh failed (credentials offline); keeping the previous catalog; \
                 check listing configuration and credentials"
            )
        );
        assert_eq!(first.refresh_in, Some(100));
        let second = state.refresh_failed("credentials offline", &settings(100), 200);
        assert_eq!(second.warning, None);
        assert_eq!(second.refresh_in, Some(100));
    }

    #[test]
    fn a_refresh_does_not_reschedule_over_a_load_already_in_flight() {
        let mut state = cache();
        let _ = state.access(&settings(100), Some("token-a"), 0);
        assert_eq!(state.refresh_completed(&settings(100), 0), None);
    }

    #[test]
    fn the_diagnostic_formats_are_the_upstream_ones() {
        assert_eq!(
            info_message("https://api.example/models", 3, 1),
            "model listing from https://api.example/models loaded 3 model(s), skipped 1 row(s)"
        );
        assert_eq!(
            failure_warning("https://api.example/models", "Error: offline", false),
            "model listing from https://api.example/models failed (Error: offline); using the \
             bundled catalog"
        );
        assert_eq!(
            failure_warning("https://api.example/models", "Error: offline", true),
            "model listing from https://api.example/models failed (Error: offline); keeping the \
             previous catalog"
        );
        assert_eq!(
            notification_failure_warning("observer failed"),
            "model listing change notification failed (observer failed); reload the model catalog"
        );
    }

    #[test]
    fn extension_fields_merge_over_the_base_request_without_mutating_it() {
        let base = json!({"model": "m", "messages": []});
        let mut fields = Map::new();
        fields.insert("metadata".to_owned(), json!({"trace": "abc"}));
        let merged = merge_request_extensions(&base, &fields).expect("no collision");
        assert_eq!(
            merged,
            json!({"model": "m", "messages": [], "metadata": {"trace": "abc"}})
        );
        assert_eq!(
            base,
            json!({"model": "m", "messages": []}),
            "the base is untouched"
        );
    }

    #[test]
    fn a_contributed_field_that_shadows_the_base_request_is_refused() {
        let base = json!({"model": "m", "messages": []});
        let mut fields = Map::new();
        fields.insert("messages".to_owned(), json!([{"role": "user"}]));
        let error = merge_request_extensions(&base, &fields).expect_err("a collision");
        assert_eq!(
            error,
            RequestExtensionError::Collision {
                field: "messages".to_owned()
            }
        );
        assert_eq!(
            error.to_string(),
            "DeepSeek request extension field \"messages\" collides with the base request"
        );
        assert_eq!(RequestExtensionError::code(), "REQUEST_EXTENSION");
    }

    #[test]
    fn presence_is_the_collision_test_not_truthiness() {
        let base = json!({"model": "m", "metadata": null});
        let mut fields = Map::new();
        fields.insert("metadata".to_owned(), json!({"trace": "abc"}));
        assert_eq!(
            colliding_field(base.as_object().expect("object"), &fields),
            Some("metadata")
        );
    }

    #[test]
    fn a_non_object_base_request_is_refused() {
        let error = merge_request_extensions(&json!([]), &Map::new()).expect_err("not an object");
        assert_eq!(error, RequestExtensionError::NotAnObject);
        assert_eq!(RequestExtensionError::code(), "REQUEST_EXTENSION");
    }
}
