//! Share links: a token that names one session, and the store that holds the tokens.
//!
//! # What this module is, and what it deliberately is not
//!
//! It issues tokens and resolves them back to a session id. It does **not** decide whether the
//! holder may read that session. Authorisation stays with the caller because it depends on facts
//! this module cannot see: who is asking, which deployment serves the link, whether sharing is
//! enabled at all, and what the session's own access policy says. A store that also answered "is
//! this allowed?" would have to duplicate the Remote layer's [`crate::access::Policy`] and would
//! then be the one place the two answers could disagree.
//!
//! The consequence is worth stating plainly: a resolved [`Share`] means *this token is live*, not
//! *this caller is authorised*. Callers must run their own check before handing out a session body —
//! and this module never touches session storage, so it cannot leak one by itself.
//!
//! Sharing is read-only by construction rather than by a flag. A [`Share`] carries a session id, a
//! creation time, and a deadline; there is no field in which a write permission could travel, so no
//! code path here can be argued into mutating a session.
//!
//! # Why the token is unguessable, and how it is built
//!
//! A token is a capability: whoever holds it is the share. That makes guessability the whole
//! security property, so the value is derived from entropy the caller injects
//! (`&dyn Fn() -> u64`) instead of from a random number generator inside this module. Two reasons,
//! both practical: injecting entropy adds no dependency (the workspace has no `rand` in
//! `girder-core`, and a token is not a reason to take one), and a test can pass a sequence that
//! makes the output reproducible — an unguessable-by-construction value is otherwise untestable
//! beyond "it looks random".
//!
//! A token is 32 bytes, written as 64 lowercase hex characters:
//!
//! - the first 24 bytes ([`TOKEN_ENTROPY_WORDS`] mixed `u64`s) carry **192 bits of entropy**;
//! - the last 8 bytes repeat the collision counter from [`Shares::create`].
//!
//! 192 bits is far past what guessing survives: an attacker gets one online attempt per request,
//! so even a billion attempts a second would run for longer than the age of the universe before a
//! 2^192 space gave a coin flip. It is also far past the birthday bound that a shorter token would
//! hit: with 64 bits, a deployment holding a few billion shares would start to see its own tokens
//! collide. Hex rather than base64 because a token must survive being pasted into a URL, a chat
//! message, a log line, and an `Authorization` header unchanged, and no encoding does that with
//! fewer special cases than plain hex.
//!
//! The entropy words are passed through a bijective mixer (`splitmix64`'s finalizer, see [`mix`])
//! before they are written out. That is not for security — XOR-ing an already uniform `u64` adds no
//! strength — it is because callers often inject *weak* entropy in tests and some platforms hand out
//! counters, and a mixer makes adjacent inputs produce unrelated-looking tokens rather than tokens
//! that differ in one hex digit.
//!
//! The trailing counter is what makes [`Shares::create`] total. A token that is already in the store
//! cannot be handed out, because it would silently grant access to someone else's session, so
//! `create` retries with fresh entropy; the counter guarantees those retries differ even when the
//! entropy source is stuck on one value, and [`Shares::create`] therefore cannot loop forever. The
//! counter is not secret and reveals nothing: it says "this was the n-th attempt", not "here is
//! entropy that was used".
//!
//! # Why expiry is decided in `resolve`, not in `create`
//!
//! `create` knows the creation time and the time-to-live; it could compute a verdict. It must not,
//! because that verdict is about the only instant it can see. A share that is valid when it is minted
//! is valid then and valid for whatever a later caller's clock says — the question "may this be
//! served *now*" can only be answered against a current reading. So `create` records the absolute
//! deadline (`created_ms + ttl_ms`) and [`Shares::resolve`] compares the caller's `now_ms` to it.
//!
//! Storing the deadline rather than a countdown matters for the same reason: a countdown has to be
//! decremented by somebody, and anyone who forgets, or decrements twice, or decrements on the wrong
//! clock, changes when a share dies. An absolute instant is judged by every reader identically.
//!
//! # Why a rewind cannot revive a share that has been pruned
//!
//! Deadline comparison alone does not survive a clock that moves backwards: if a share expired at
//! T and the clock is later set to T-1, the comparison says "live". [`Shares::prune`] is the answer,
//! and it is why `prune` *removes* entries instead of marking them: once a share is gone from the
//! store, no subsequent reading of any clock can resolve it. A deployment that needs the stronger
//! property — an expired share never coming back — prunes before it serves, not merely when it
//! happens to feel like housekeeping. Within an unpruned store the honest statement is the weaker
//! one, and it is recorded here rather than left for someone to discover.
//!
//! # Why lookups are in a `BTreeMap`
//!
//! Tokens are looked up by a value the caller supplies, and a hash map's cost depends on that value.
//! A `BTreeMap` has no adversarial worst case to exploit, and it gives `prune` a deterministic order,
//! which is what makes the removal count in its tests reproducible. The comparison inside is not
//! constant-time; with 192 bits of entropy behind every token, a timing oracle would have to extract
//! many bits per byte from network-timed responses to be useful, and a suspicious token can simply be
//! revoked. Store sizes here are small enough that the log factor is not a consideration.

use std::collections::BTreeMap;

/// How many `u64`s of entropy go into one token.
///
/// Three words are 24 bytes, 192 bits: enough that guessing is hopeless and short of the 32 bytes a
/// token occupies, which leaves room for the collision counter that makes creation total.
pub const TOKEN_ENTROPY_WORDS: usize = 3;

/// The number of bytes a token occupies before it is encoded.
pub const TOKEN_BYTES: usize = 32;

/// The number of characters in an encoded token: two hex digits per byte.
pub const TOKEN_HEX_LEN: usize = TOKEN_BYTES * 2;

/// The bytes at the end of a token that carry the collision counter.
const COUNTER_BYTES: usize = 8;

/// One share link: the token and what it points at.
///
/// Fields are public because the record is data, not behaviour: a caller rendering a link, writing a
/// row, or auditing an old share needs to read every field, and accessors that only returned copies
/// would add nothing. Nothing here is mutable after creation — a share is revoked or pruned, never
/// edited — which is what keeps `token` and `session_id` from drifting apart.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Share {
    /// The unguessable token.
    ///
    /// This is a bearer secret: anyone holding it can present it to whatever serves shares. It
    /// therefore belongs in a URL fragment or a body, not in a log line, and the caller should treat
    /// printing it as equivalent to printing the session.
    pub token: String,

    /// The session this share points at.
    ///
    /// Stored as given. This module has no access to a session catalog, so it cannot tell a real id
    /// from a typo; naming a session that does not exist is a caller error that shows up as a failed
    /// read, not as a failed share.
    pub session_id: String,

    /// When the share was created, in milliseconds since the Unix epoch.
    ///
    /// Kept for display and audit. It is also the base the deadline is derived from, so a share whose
    /// creation time is unknown could not be explained after the fact.
    pub created_ms: u64,

    /// When the share stops being live, in milliseconds since the Unix epoch.
    ///
    /// `None` means it never expires, which is a deliberate choice a caller makes rather than a
    /// default this module applies: a share with no deadline is one nobody can recall.
    pub expires_ms: Option<u64>,
}

impl Share {
    /// Whether this share is past its deadline at `now_ms`.
    ///
    /// The comparison is inclusive — `now_ms == deadline` counts as expired — because a deadline is
    /// the first instant at which the share is no longer live. An exclusive comparison would hand out
    /// one extra millisecond, and "expires at T" would then be off by one in every log line that
    /// quoted it.
    #[must_use]
    pub fn is_expired(&self, now_ms: u64) -> bool {
        self.expires_ms.is_some_and(|at| now_ms >= at)
    }
}

/// Why a token could not be resolved.
///
/// Two variants, because the caller's next move differs: an unknown token is someone presenting a
/// link that never existed or was revoked, while an expired one names the deadline it passed. Merging
/// them into one "not found" would leave a support conversation unable to distinguish a mistyped link
/// from a share that aged out — and their server-side responses should be the same anyway, which is
/// the caller's decision to make, not this type's.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum ShareError {
    /// No share has this token: never created, already pruned, or revoked.
    #[error("no share has this token")]
    Unknown,

    /// The share exists but its deadline has passed.
    #[error("the share expired at {at} ms")]
    Expired {
        /// The deadline the share passed, in milliseconds since the Unix epoch.
        ///
        /// Reported rather than recomputed so a client can say "expired at" with the value the store
        /// actually used.
        at: u64,
    },
}

/// The shares a deployment currently honours, keyed by token.
///
/// An owned store rather than a global one, matching the rest of the crate: a deployment that ran two
/// harnesses in one process would otherwise have them answer for each other's sessions. Cloning is
/// not offered, because two copies of a capability store drift the moment one of them is revoked.
#[derive(Debug, Default)]
pub struct Shares {
    /// Token to share. The token is the key *and* the field, so a lookup hit needs no second index;
    /// both are written from the same value in [`Shares::create`] and never updated.
    by_token: BTreeMap<String, Share>,
}

impl Shares {
    /// An empty store.
    #[must_use]
    pub const fn new() -> Self {
        Self {
            by_token: BTreeMap::new(),
        }
    }

    /// How many shares are held, expired-but-unpruned ones included.
    ///
    /// Expired entries count because they are still here: this is the size of the store, not the
    /// number of links that would resolve. Reporting the latter would hide the leak `prune` exists to
    /// fix.
    #[must_use]
    pub fn len(&self) -> usize {
        self.by_token.len()
    }

    /// Whether the store holds no shares at all.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.by_token.is_empty()
    }

    /// Creates a share for `session_id`.
    ///
    /// `ttl_ms` is measured from `created_ms`, not from the wall clock: the caller supplies the
    /// instant, so a test, a replay, or an import can mint a share with the timing it means. A `ttl_ms`
    /// of zero gives a share that is already expired; it is stored and reported by
    /// [`Shares::resolve`] rather than dropped, because a `create` that silently refused would be a
    /// `create` whose return value did not describe the store.
    ///
    /// `entropy` is called once per [`TOKEN_ENTROPY_WORDS`] per attempt and must be unpredictable for
    /// the token to be unguessable; a deterministic source is fine for tests and fatal for production.
    /// See the module documentation for what the token is made of.
    ///
    /// A collision with an existing token is retried with fresh entropy. At most `len + 1` attempts
    /// are needed: the counter in the token makes every attempt distinct, and at most `len` of those
    /// attempts can name a held share.
    pub fn create(
        &mut self,
        session_id: &str,
        created_ms: u64,
        ttl_ms: Option<u64>,
        entropy: &dyn Fn() -> u64,
    ) -> Share {
        // Saturating rather than wrapping: a `ttl_ms` large enough to overflow would otherwise wrap to
        // an instant in the past and mint a share that is dead on arrival, and a caller who asked for
        // "a very long time" would get "no time at all". Pinning to `u64::MAX` is still a deadline,
        // still finite, and still the caller's stated intent in the only way this type can express it.
        let expires_ms = ttl_ms.map(|ttl| created_ms.saturating_add(ttl));

        let attempts = u64::try_from(self.by_token.len()).map_or(u64::MAX, |len| len);
        for attempt in 0..=attempts {
            let token = derive_token(entropy, attempt);
            if self.by_token.contains_key(&token) {
                continue;
            }
            let share = Share {
                token: token.clone(),
                session_id: session_id.to_owned(),
                created_ms,
                expires_ms,
            };
            self.by_token.insert(token, share.clone());
            return share;
        }

        // Unreachable while the argument above holds: `attempts + 1` distinct tokens cannot all be
        // held by a store of `attempts` entries. Kept as a real branch rather than an `expect` so a
        // future change to the token layout degrades to a share with a distinct token instead of a
        // panic in a request path.
        let token = derive_token(entropy, u64::MAX);
        let share = Share {
            token: token.clone(),
            session_id: session_id.to_owned(),
            created_ms,
            expires_ms,
        };
        self.by_token.insert(token, share.clone());
        share
    }

    /// Resolves `token` to a live share.
    ///
    /// `now_ms` is the caller's current instant, and it is the *only* thing expiry is judged against;
    /// see the module documentation for why that decision lives here. Resolving does not touch the
    /// store, so a caller holding `&Shares` can serve a request without a lock that a revocation would
    /// block on.
    ///
    /// A resolved share says the token is live, not that the caller may read the session: permission
    /// is the caller's check.
    pub fn resolve(&self, token: &str, now_ms: u64) -> Result<&Share, ShareError> {
        let share = self.by_token.get(token).ok_or(ShareError::Unknown)?;
        match share.expires_ms {
            Some(at) if now_ms >= at => Err(ShareError::Expired { at }),
            _ => Ok(share),
        }
    }

    /// Revokes one share, reporting whether it was there.
    ///
    /// The token is removed rather than flagged, for the same reason [`Shares::prune`] removes: a
    /// revoked capability should not be recoverable by any clock, and a flag is a second state that
    /// every reader has to remember to honour. `false` means the token was already gone — a retry, or
    /// a caller who never had one — which is a finding, not an error, so it is not a `Result`.
    pub fn revoke(&mut self, token: &str) -> bool {
        self.by_token.remove(token).is_some()
    }

    /// Removes every share whose deadline has passed at `now_ms`, returning how many were removed.
    ///
    /// Removal is physical: the entries are gone from the map, so a later clock reading cannot
    /// resolve them. This is the operation that makes the "an expired share never comes back" promise
    /// true across a clock rewind — the comparison in [`Shares::resolve`] alone cannot, because a
    /// clock set backwards makes an unpruned expired share look live again.
    ///
    /// The count is returned so a sweeper can log what it reclaimed and so an idle period is
    /// distinguishable from one that removed data.
    pub fn prune(&mut self, now_ms: u64) -> usize {
        let before = self.by_token.len();
        // `retain` visits every entry once and drops in place; collecting keys first would allocate a
        // second copy of what is usually the whole store.
        self.by_token.retain(|_, share| !share.is_expired(now_ms));
        before - self.by_token.len()
    }
}

/// Derives the token for one creation attempt.
///
/// The layout — mixed entropy, then the attempt counter — is documented on the module. The counter is
/// written last so that two attempts can never produce the same token even when `entropy` returns one
/// constant, which is the property [`Shares::create`] relies on to terminate.
fn derive_token(entropy: &dyn Fn() -> u64, attempt: u64) -> String {
    let mut bytes = [0_u8; TOKEN_BYTES];
    let entropy_end = TOKEN_BYTES - COUNTER_BYTES;

    for (index, chunk) in bytes[..entropy_end].chunks_exact_mut(8).enumerate() {
        // The index is a `u64` from a loop bounded by `TOKEN_ENTROPY_WORDS`; the fallback exists so the
        // conversion cannot panic, and it cannot be reached.
        let word = u64::try_from(index).map_or(u64::MAX, |index| index);
        chunk.copy_from_slice(&mix(entropy() ^ mix(word)).to_be_bytes());
    }
    bytes[entropy_end..].copy_from_slice(&attempt.to_be_bytes());

    encode_hex(&bytes)
}

/// Mixes one `u64` into an unrelated one.
///
/// This is `splitmix64`'s finalizer, chosen because it is short, well studied, and — the property that
/// matters here — a **bijection**: every step is either an odd multiply (invertible modulo 2^64) or an
/// XOR-shift (its own inverse at the same distance), so distinct inputs always give distinct outputs.
/// That is what lets a stuck entropy source still produce distinct tokens across attempts. It is a
/// mixer, not a hash: it adds no entropy and no preimage resistance, and nothing in this module
/// depends on either.
fn mix(mut value: u64) -> u64 {
    value = value.wrapping_add(0x9E37_79B9_7F4A_7C15);
    value = (value ^ (value >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
    value = (value ^ (value >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
    value ^ (value >> 31)
}

/// Writes bytes as lowercase hex.
///
/// Hand-rolled because it is twelve lines and a dependency for it would be a dependency taken on in
/// every build of this crate. Lowercase specifically: a token that is sometimes `AB` and sometimes
/// `ab` is two tokens to every store that compares strings, including this one.
fn encode_hex(bytes: &[u8]) -> String {
    const DIGITS: [u8; 16] = *b"0123456789abcdef";
    let mut out = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        let high = usize::from(byte >> 4);
        let low = usize::from(byte & 0x0F);
        out.push(char::from(DIGITS[high]));
        out.push(char::from(DIGITS[low]));
    }
    out
}
