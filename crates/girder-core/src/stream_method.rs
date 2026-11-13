//! Streaming methods: the declaration, the open handle, and the ceiling on how many may be open.
//!
//! [`RemoteRegistry`](crate::remote::RemoteRegistry) is a request/response surface: a method takes one
//! `RemoteCall` and answers with one [`Value`](serde_json::Value). Upstream also has methods that keep
//! producing — `session.stream`, `terminal.attach`, `job.watch` — and this build answers those by
//! polling instead (`session.follow`, `job.follow`). Polling is a protocol of its own; it cannot be
//! layered on a table whose every entry returns exactly once.
//!
//! This module is the first half of changing that: a method can be *declared* streaming, and a
//! declared method can be *opened* into a handle a caller can hold. It deliberately does **not**
//! carry a producer/consumer queue. Admission and delivery are separable, and only one of them is
//! safe to add today: see "What this module does not do" below.
//!
//! # Declaration is static; opening is runtime
//!
//! [`Streams::declare`] describes what a method *is*: a qualified name and the content type of one
//! frame. It runs while the method table is assembled, once, before any client is connected.
//! [`Streams::open`] describes what a client *did*: it hands back a [`StreamHandle`] or refuses.
//!
//! The two are kept apart on purpose. "Does this method stream?" must be answerable without
//! allocating anything, because it is asked on every capability probe and in every generated client
//! catalog. "Is this stream open?" must be answerable without consulting the method table, because
//! it is asked when a connection tears down. Collapsing them — for example by letting `open` treat
//! the first call as the declaration — would make a typo indistinguishable from a first open, and
//! would let a client mint methods the server never offered.
//!
//! # Why a ceiling, and why it belongs here
//!
//! A stream is a resource the server holds on a client's behalf: an id, a buffer, and whatever
//! produced it. A client that opens a thousand of them and never closes one exhausts the process,
//! and the transport cannot prevent it because every individual open is a legal request. The
//! ceiling therefore has to sit next to the counter that the opens increment, which is exactly this
//! table.
//!
//! The same failure mode is already recorded, at the transport layer, in
//! `docs/rpc-limits-finding.md`: an unbounded outbound queue lets one client that writes without
//! reading grow the server's memory until it dies. The finding is about *bytes* per connection; this
//! is about *streams* per manager. Both are "no ceiling, no service", and both are fixed the same
//! way — a number that is decided in advance rather than discovered by an `OOM` kill. The sibling
//! primitive is [`Limiter`](crate::rpc_limits::Limiter), which caps requests; `max_open` caps streams.
//!
//! `max_open` is immutable after construction. A limit a request could raise is not a limit.
//!
//! # Why `close` consumes the handle
//!
//! [`Streams::close`] takes `StreamHandle` by value, and `StreamHandle` is neither `Clone` nor
//! `Copy`. That makes a second close of the same stream a *compile* error rather than a runtime
//! no-op, which is the difference between a bug that is caught before merge and one that shows up as
//! an under-counted table under load. A runtime "already closed" check would still allow the caller
//! to keep using a handle it no longer owns, and the table would only learn about the mistake when
//! the count drifted.
//!
//! # What a dropped handle does not do
//!
//! Dropping a [`StreamHandle`] without closing it does **not** release the slot; the stream stays
//! open until something closes it, and [`Streams::open_count`] will keep counting it. That is the
//! cost of the ownership design and it is stated rather than hidden: an implicit close-on-drop would
//! fire while a transport still holds the id it is mid-way through reading, turning "the caller let
//! the handle go out of scope" into a closed stream under a live reader. The teardown path — the
//! connection owner — is responsible for calling `close`, and the leak is visible in `open_count`
//! instead of corrupting the table.
//!
//! # What this module does not do
//!
//! - It stores no frames and wakes no reader. A streaming method needs a bounded producer/consumer
//!   pair as well; that is the other half, and it should be designed against a bounded channel, not
//!   added on top of the unbounded one `docs/rpc-limits-finding.md` measures.
//! - It is not wired into [`RemoteRegistry`](crate::remote::RemoteRegistry). Registering a method
//!   that returns a handle is a change to `remote.rs`; exporting this module is a change to `lib.rs`.
//!   Both are separate, reviewable steps — the same arrangement [`rpc_limits`](crate::rpc_limits)
//!   and [`bounded_queue`](crate::bounded_queue) are in.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Mutex, MutexGuard, PoisonError};

/// Distinguishes one stream's id from every other stream's, in this process.
///
/// Process-wide rather than per [`Streams`] so two managers cannot mint the same id: a handle handed
/// to the wrong manager then removes nothing, instead of closing an unrelated stream that happens to
/// be using the same number.
static NEXT_ID: AtomicU64 = AtomicU64::new(1);

/// A method that produces a sequence of frames rather than one response.
///
/// Built by the code that assembles the method table, not by a client: the fields are public because
/// the declaration is data, but the only path that makes one *count* is [`Streams::declare`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct StreamMethod {
    /// Qualified `namespace.method`, spelled the way the rest of the surface spells it.
    pub name: String,
    /// The content type of one frame, used by the client to decode what arrives.
    ///
    /// Empty is refused at declaration: a frame type nobody can name is a frame the client cannot
    /// decode, and accepting it would defer that failure to the first frame instead of start-up.
    pub frame_type: String,
}

/// An open stream.
///
/// Deliberately not `Clone`: the handle is the proof that the bearer may close this stream, and two
/// copies of a proof would permit the second close that `close` taking the handle by value exists to
/// prevent. Not `Copy` for the same reason.
#[derive(Debug)]
pub struct StreamHandle {
    /// Unique for the life of the process, and prefixed with the method name so a log line or an
    /// error message identifies the stream without a lookup.
    id: String,
    /// The frame type the declaration gave this method, carried so the encoding path does not have
    /// to re-read the declaration table under the lock while it is writing frames.
    frame_type: String,
}

impl StreamHandle {
    /// The stream's identifier.
    ///
    /// Exposed because it is the only way a caller can ask [`Streams::is_open`] about this stream,
    /// or mention it in a log without inventing a second naming scheme.
    #[must_use]
    pub fn id(&self) -> &str {
        &self.id
    }

    /// The content type of one frame on this stream.
    #[must_use]
    pub fn frame_type(&self) -> &str {
        &self.frame_type
    }
}

/// Why a stream could not be opened.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum StreamError {
    /// The manager already holds as many open streams as it allows.
    ///
    /// Distinct from [`StreamError::NotStreaming`] because the two mean different things to a
    /// client: one says "close something and retry", the other says "this call will never work".
    #[error("too many open streams (limit {limit})")]
    TooMany {
        /// The ceiling, so the client can tell "wait for a slot" from "this will never fit".
        limit: usize,
    },

    /// The method never declared itself streaming.
    #[error("`{method}` is not a streaming method")]
    NotStreaming {
        /// The name that was asked for, echoed so the client sees which declaration is missing.
        method: String,
    },
}

/// The stream table for one server, or one connection.
///
/// Shared, not owned: every method takes `&self` and the mutable state sits behind one mutex, because
/// the transport hands the same table to every connection task. A `&mut self` API would force the
/// caller to serialize connections it does not otherwise need to serialize.
#[derive(Debug)]
pub struct Streams {
    /// The ceiling on simultaneously open streams. Fixed at construction; see the module docs.
    max_open: usize,
    /// Declarations and open ids behind a single lock, so the ceiling check and the insert that
    /// depends on it cannot be separated by another thread's close.
    state: Mutex<State>,
}

/// Everything the table guards.
#[derive(Debug)]
struct State {
    /// Declared methods, by qualified name, mapped to their frame type.
    ///
    /// Only the frame type is stored: the name is the key, and copying it into the value would give
    /// two places to keep in agreement.
    declared: BTreeMap<String, String>,
    /// Ids of streams open right now. A set, because the question this table answers about an open
    /// stream is "exists?" and never "what does it contain?".
    open: BTreeSet<String>,
}

impl Streams {
    /// A table that allows at most `max_open` streams at once, with nothing declared yet.
    ///
    /// Zero is a valid ceiling and means "refuse every stream": useful for a deployment that does
    /// not want the upstream streaming surface, and for tests. It is not clamped to one, because
    /// silently substituting a different number for a configured one hides a misconfiguration
    /// instead of reporting it.
    #[must_use]
    pub fn new(max_open: usize) -> Self {
        Self {
            max_open,
            state: Mutex::new(State {
                declared: BTreeMap::new(),
                open: BTreeSet::new(),
            }),
        }
    }

    /// Declares `method` as streaming, and reports whether the declaration was stored.
    ///
    /// `false` means the declaration was unusable — an empty or unqualified name, a name with
    /// whitespace in it, or an empty frame type — and was therefore not stored, so [`Streams::open`]
    /// will report the method as not streaming. A `bool` rather than an error because the only
    /// caller is start-up registration, where a `false` is a bug in this build rather than a
    /// condition any client caused, and because the enum of client-facing errors should not grow a
    /// variant no request can produce.
    ///
    /// Declaring the same name twice replaces the frame type: redeclaration is how a build corrects
    /// a stale value, and refusing it would leave the old one in place with no way to say so.
    pub fn declare(&self, method: StreamMethod) -> bool {
        if !is_addressable(&method.name) || method.frame_type.trim().is_empty() {
            return false;
        }
        self.lock().declared.insert(method.name, method.frame_type);
        true
    }

    /// Whether `name` declared itself streaming.
    ///
    /// Static: this answers "does the method have a stream kind?", not "is a stream open?", which is
    /// [`Streams::is_open`]. Answering the first question must not allocate or reserve anything,
    /// because a client catalog asks it for every method it lists.
    #[must_use]
    pub fn is_streaming(&self, name: &str) -> bool {
        self.lock().declared.contains_key(name)
    }

    /// Opens a stream on a declared method.
    ///
    /// The frame type recorded on the handle is the one from the declaration, not the one in
    /// `method`. The declaration is what a client decoded its catalog from, so it is the only copy
    /// two call sites cannot disagree about; `method` names *which* declaration to open.
    ///
    /// Two checks, in this order: a method that does not stream is refused before capacity is
    /// consulted, because "this method has no stream kind" is a protocol error that retrying cannot
    /// fix, while "no slot right now" is one that waiting can.
    pub fn open(&self, method: &StreamMethod) -> Result<StreamHandle, StreamError> {
        // Checked before the table is consulted: a name that cannot address a method cannot have
        // been declared, and reporting it as "not streaming" is the same answer a client gets for
        // any method that never declared itself.
        if !is_addressable(&method.name) {
            return Err(StreamError::NotStreaming {
                method: method.name.clone(),
            });
        }

        let mut state = self.lock();
        let Some(frame_type) = state.declared.get(&method.name).cloned() else {
            return Err(StreamError::NotStreaming {
                method: method.name.clone(),
            });
        };

        if state.open.len() >= self.max_open {
            return Err(StreamError::TooMany {
                limit: self.max_open,
            });
        }

        let id = mint_id(&state.open, &method.name);
        state.open.insert(id.clone());

        Ok(StreamHandle { id, frame_type })
    }

    /// Closes a stream, consuming the handle.
    ///
    /// Consuming it is the mechanism, not a side effect: after this call the handle no longer
    /// exists, so a second close is a compile error. See the module docs for why that is preferable
    /// to a runtime check.
    ///
    /// A handle minted by a different manager removes nothing here and is ignored. There is no
    /// failure to report — the caller cannot act on "that was not one of mine" — and silence is
    /// safer than a panic on a teardown path.
    pub fn close(&self, handle: StreamHandle) {
        let StreamHandle { id, .. } = handle;
        self.lock().open.remove(&id);
    }

    /// How many streams are open right now.
    #[must_use]
    pub fn open_count(&self) -> usize {
        self.lock().open.len()
    }

    /// Whether the stream with this id is open.
    #[must_use]
    pub fn is_open(&self, id: &str) -> bool {
        self.lock().open.contains(id)
    }

    /// Locks the state, recovering from a poisoned mutex.
    ///
    /// The guarded data is a set and a map of plain values: a panic while holding the lock leaves
    /// them consistent. Propagating the poison would let one panicked request disable streaming for
    /// every later connection, which is a larger failure than the one that caused it.
    fn lock(&self) -> MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }
}

/// Whether `name` could address a method at all.
///
/// The qualified form is `namespace.method`, as the method table and the subscription check both
/// use. An empty name, a name with whitespace in it, or a name with no `.` is not a method anyone can
/// call, so it cannot be a streaming one either — and letting it through would produce ids like
/// `#7`, which no log line can be traced back to a method with.
fn is_addressable(name: &str) -> bool {
    !name.is_empty()
        && name.trim() == name
        && !name.chars().any(char::is_whitespace)
        && name.contains('.')
}

/// Mints an id no open stream is using.
///
/// The counter is process-wide, so two managers never collide. A `u64` that wraps would eventually
/// offer an id that is still open; skipping a taken id costs one comparison and removes the aliasing
/// case outright, at the price of a loop whose body runs at most `open.len() + 1` times.
fn mint_id(open: &BTreeSet<String>, name: &str) -> String {
    loop {
        let sequence = NEXT_ID.fetch_add(1, Ordering::Relaxed);
        let candidate = format!("{name}#{sequence}");
        if !open.contains(&candidate) {
            return candidate;
        }
    }
}
