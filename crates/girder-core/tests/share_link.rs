//! Tests for share links (`share_link`).
//!
//! The properties under test are the ones a capability store gets wrong quietly. A round trip that
//! works proves almost nothing; what matters is that a share stops resolving the instant its deadline
//! passes, that a revoked one cannot be recovered, that `prune` really removes rather than masks, and
//! that two different entropy readings never name the same session.
//!
//! The module is compiled into this test binary through `#[path]` because exporting it from `lib.rs`
//! is a separate change, which is also why nothing here reaches for the rest of `girder_core`: the
//! only dependency is the module itself.

#[path = "../src/share_link.rs"]
mod share_link;

use std::cell::Cell;

use share_link::{ShareError, Shares, TOKEN_ENTROPY_WORDS, TOKEN_HEX_LEN};

/// A deterministic entropy source.
///
/// It walks a small fixed sequence and then repeats its last value, so a test can name exactly how
/// many words it expects to be consumed and can also hand out a *stuck* source on purpose — the case
/// the collision counter exists for.
fn source(values: &[u64]) -> impl Fn() -> u64 + '_ {
    let index = Cell::new(0_usize);
    move || {
        let at = index.get();
        index.set(at + 1);
        values.get(at).copied().unwrap_or_else(|| {
            // Repeating the tail rather than panicking keeps the "stuck source" test honest: it is
            // the same closure that has to keep returning one value.
            values.last().copied().unwrap_or(0)
        })
    }
}

/// A source that always returns the same word — the worst case a caller can hand us.
fn stuck(value: u64) -> impl Fn() -> u64 {
    move || value
}

/// A source with enough distinct words for one creation.
fn distinct() -> impl Fn() -> u64 {
    source(&[1, 2, 3, 4, 5, 6, 7, 8, 9])
}

/// One second, in the unit every instant here is expressed in.
const SECOND: u64 = 1_000;

// ---- creating and resolving --------------------------------------------------------------------

#[test]
fn a_created_share_resolves_to_the_same_session() {
    let mut shares = Shares::new();
    let share = shares.create("session-a", 10_000, Some(60 * SECOND), &distinct());

    let resolved = shares
        .resolve(&share.token, 10_000)
        .expect("a fresh share resolves");

    assert_eq!(resolved.session_id, "session-a");
    assert_eq!(resolved.created_ms, 10_000);
    assert_eq!(resolved.expires_ms, Some(10_000 + 60 * SECOND));
    assert_eq!(resolved.token, share.token);
}

#[test]
fn a_share_without_a_ttl_never_expires() {
    // `None` is a real choice, not a missing value: no reading of the clock may retire it.
    let mut shares = Shares::new();
    let share = shares.create("session-a", 10_000, None, &distinct());

    assert_eq!(share.expires_ms, None);
    assert!(!share.is_expired(u64::MAX));
    assert_eq!(
        shares
            .resolve(&share.token, u64::MAX)
            .expect("no deadline means no expiry")
            .session_id,
        "session-a"
    );
}

#[test]
fn the_newest_share_is_the_whole_store() {
    let mut shares = Shares::new();
    assert!(shares.is_empty());

    let share = shares.create("session-a", 0, None, &distinct());

    assert_eq!(shares.len(), 1);
    assert!(!shares.is_empty());
    assert!(shares.resolve(&share.token, 0).is_ok());
}

// ---- the token itself --------------------------------------------------------------------------

#[test]
fn a_token_is_the_advertised_length_and_lowercase_hex() {
    // The length and alphabet are part of the contract: a token goes into URLs and log lines, and a
    // mixed-case or differently-sized token breaks every store that compares strings.
    let mut shares = Shares::new();
    let share = shares.create("session-a", 0, None, &distinct());

    assert_eq!(share.token.len(), TOKEN_HEX_LEN);
    assert!(
        share
            .token
            .chars()
            .all(|c| c.is_ascii_digit() || ('a'..='f').contains(&c)),
        "token must be lowercase hex, got `{}`",
        share.token
    );
}

#[test]
fn different_entropy_produces_different_tokens() {
    // The core security property. Two shares of the same session, at the same instant, with the same
    // ttl, must still be distinguishable — otherwise handing one out would hand out the other.
    let mut shares = Shares::new();

    let first = shares.create("session-a", 0, None, &source(&[1, 2, 3]));
    let second = shares.create("session-a", 0, None, &source(&[4, 5, 6]));

    assert_ne!(first.token, second.token);
    assert_ne!(
        first.token, second.token,
        "the same token twice would make one share's revocation revoke the other"
    );
    // Both are live and each names its own record, so the difference is not merely cosmetic.
    assert_eq!(
        shares
            .resolve(&first.token, 0)
            .expect("first resolves")
            .token,
        first.token
    );
    assert_eq!(
        shares
            .resolve(&second.token, 0)
            .expect("second resolves")
            .token,
        second.token
    );
    assert_eq!(shares.len(), 2);
}

#[test]
fn one_stuck_entropy_word_still_yields_a_fresh_token() {
    // The collision counter's job: a caller that wired a constant into `entropy` gets distinct tokens
    // rather than an infinite loop or a silent overwrite of someone else's share.
    let mut shares = Shares::new();

    let first = shares.create("session-a", 0, None, &stuck(7));
    let second = shares.create("session-b", 0, None, &stuck(7));
    let third = shares.create("session-c", 0, None, &stuck(7));

    assert_ne!(first.token, second.token);
    assert_ne!(second.token, third.token);
    assert_ne!(first.token, third.token);
    assert_eq!(shares.len(), 3);
    // Each token still resolves to the session it was minted for, which is what a shared token would
    // have destroyed.
    assert_eq!(
        shares
            .resolve(&second.token, 0)
            .expect("second resolves")
            .session_id,
        "session-b"
    );
}

#[test]
fn entropy_is_consumed_one_word_per_token_position() {
    // Pins the layout from the outside: a token built from fewer words than advertised is a token
    // with fewer bits than the documentation claims.
    let words = Cell::new(0_usize);
    let counting = || {
        words.set(words.get() + 1);
        u64::try_from(words.get()).unwrap_or(0)
    };

    let mut shares = Shares::new();
    let _ = shares.create("session-a", 0, None, &counting);

    assert_eq!(words.get(), TOKEN_ENTROPY_WORDS);
}

// ---- expiry ------------------------------------------------------------------------------------

#[test]
fn a_share_resolves_right_up_to_its_deadline() {
    // The boundary is the interesting case: "expires at T" must mean the share is live before T.
    let mut shares = Shares::new();
    let share = shares.create("session-a", 1_000, Some(SECOND), &distinct());

    assert!(shares.resolve(&share.token, 1_000).is_ok());
    assert!(shares.resolve(&share.token, 1_999).is_ok());
}

#[test]
fn an_expired_share_fails_to_resolve() {
    // The core property of the module. The share is still in the store — expiry is a resolve-time
    // verdict, not a delete — and it must nevertheless be refused.
    let mut shares = Shares::new();
    let share = shares.create("session-a", 1_000, Some(SECOND), &distinct());

    let error = shares
        .resolve(&share.token, 1_000 + SECOND)
        .expect_err("a share at its deadline is no longer live");

    assert_eq!(
        error,
        ShareError::Expired { at: 1_000 + SECOND },
        "the failure must name the deadline the store actually used"
    );
    // One millisecond earlier it was live; the boundary is the deadline itself, not a guess near it.
    assert!(shares.resolve(&share.token, 1_000 + SECOND - 1).is_ok());
    // And expiry did not silently remove it.
    assert_eq!(shares.len(), 1);
}

#[test]
fn a_zero_ttl_share_is_born_expired() {
    // Storing it rather than refusing it keeps `create`'s return value a description of the store: a
    // caller that asked for a dead share gets one, and learns so from `resolve`.
    let mut shares = Shares::new();
    let share = shares.create("session-a", 5_000, Some(0), &distinct());

    assert_eq!(share.expires_ms, Some(5_000));
    assert_eq!(
        shares.resolve(&share.token, 5_000),
        Err(ShareError::Expired { at: 5_000 })
    );
}

#[test]
fn a_ttl_that_overflows_saturates_instead_of_wrapping() {
    // A wrapping add would turn "as long as possible" into a deadline in the past — a share that dies
    // the moment it is created, with no error to explain it.
    let mut shares = Shares::new();
    let share = shares.create("session-a", 1_000, Some(u64::MAX), &distinct());

    assert_eq!(share.expires_ms, Some(u64::MAX));
    assert!(shares.resolve(&share.token, u64::MAX - 1).is_ok());
    assert_eq!(
        shares.resolve(&share.token, u64::MAX),
        Err(ShareError::Expired { at: u64::MAX })
    );
}

// ---- unknown tokens ----------------------------------------------------------------------------

#[test]
fn resolving_an_unknown_token_reports_unknown() {
    // An empty store and a store with other shares must both be honest: "no such token" is not the
    // same finding as "expired", and merging them would hide a mistyped link.
    let mut shares = Shares::new();
    assert_eq!(shares.resolve("", 0), Err(ShareError::Unknown));
    assert_eq!(
        shares.resolve(
            "0000000000000000000000000000000000000000000000000000000000000000",
            0
        ),
        Err(ShareError::Unknown)
    );

    let _ = shares.create("session-a", 0, None, &distinct());

    assert_eq!(shares.resolve("not-a-token", 0), Err(ShareError::Unknown));
}

#[test]
fn a_token_is_matched_exactly() {
    // A prefix, a suffix, or a case change is a different token — anything else would let a truncated
    // link resolve to a live share.
    let mut shares = Shares::new();
    let share = shares.create("session-a", 0, None, &distinct());
    let token = share.token.clone();

    assert!(shares.resolve(&token, 0).is_ok());
    assert_eq!(
        shares.resolve(&token[..token.len() - 1], 0),
        Err(ShareError::Unknown)
    );
    assert_eq!(
        shares.resolve(&format!("{token}0"), 0),
        Err(ShareError::Unknown)
    );
    assert_eq!(
        shares.resolve(&token.to_uppercase(), 0),
        Err(ShareError::Unknown)
    );
}

// ---- revocation --------------------------------------------------------------------------------

#[test]
fn a_revoked_share_fails_to_resolve() {
    let mut shares = Shares::new();
    let share = shares.create("session-a", 0, None, &distinct());

    assert!(shares.revoke(&share.token), "the token was there");
    assert_eq!(shares.resolve(&share.token, 0), Err(ShareError::Unknown));
    assert!(shares.is_empty());
}

#[test]
fn revoking_an_absent_share_reports_false() {
    // Revocation is idempotent from the caller's side: a retry is a no-op, not a failure, and it must
    // not disturb the shares that are still live.
    let mut shares = Shares::new();
    let kept = shares.create("session-a", 0, None, &distinct());
    let removed = shares.create("session-b", 0, None, &distinct());

    assert!(shares.revoke(&removed.token));
    assert!(!shares.revoke(&removed.token));
    assert!(!shares.revoke("no-such-token"));
    assert_eq!(shares.len(), 1);
    assert!(shares.resolve(&kept.token, 0).is_ok());
}

// ---- pruning -----------------------------------------------------------------------------------

#[test]
fn prune_removes_expired_shares_and_keeps_live_ones() {
    // "Removes" is the point. A marking prune would leave the entry resolvable by any clock that
    // moved backwards, which is exactly the hole prune exists to close.
    let mut shares = Shares::new();
    let old = shares.create("session-old", 0, Some(SECOND), &source(&[1, 2, 3]));
    let fresh = shares.create("session-fresh", 10_000, Some(SECOND), &source(&[4, 5, 6]));

    let removed = shares.prune(2_000);

    assert_eq!(removed, 1);
    assert_eq!(shares.len(), 1);
    assert_eq!(
        shares.resolve(&old.token, 2_000),
        Err(ShareError::Unknown),
        "a pruned token must be gone, not merely skipped"
    );
    assert!(shares.resolve(&fresh.token, 2_000).is_ok());
}

#[test]
fn a_pruned_share_does_not_come_back_when_the_clock_moves_backwards() {
    // The reason prune deletes. The deadline comparison in `resolve` alone cannot survive a rewound
    // clock; removal can, because there is nothing left to compare.
    let mut shares = Shares::new();
    let share = shares.create("session-a", 10_000, Some(SECOND), &distinct());

    assert_eq!(
        shares.resolve(&share.token, 20_000),
        Err(ShareError::Expired { at: 11_000 })
    );
    assert_eq!(shares.prune(20_000), 1);
    assert_eq!(
        shares.resolve(&share.token, 10_000),
        Err(ShareError::Unknown)
    );
}

#[test]
fn prune_keeps_shares_without_a_deadline() {
    let mut shares = Shares::new();
    let forever = shares.create("session-a", 0, None, &distinct());

    assert_eq!(shares.prune(u64::MAX - 1), 0);
    assert_eq!(shares.len(), 1);
    assert!(shares.resolve(&forever.token, u64::MAX - 1).is_ok());
}

#[test]
fn prune_on_an_empty_store_removes_nothing() {
    let mut shares = Shares::new();

    assert_eq!(shares.prune(0), 0);
    assert_eq!(shares.prune(u64::MAX), 0);
    assert!(shares.is_empty());
}

#[test]
fn prune_at_the_deadline_removes_the_share() {
    // The boundary again, from the pruning side: the deadline instant is already too late.
    let mut shares = Shares::new();
    let share = shares.create("session-a", 1_000, Some(SECOND), &distinct());

    assert_eq!(shares.prune(2_000 - 1), 0);
    assert_eq!(
        shares
            .resolve(&share.token, 2_000 - 1)
            .expect("still live")
            .token,
        share.token
    );

    assert_eq!(shares.prune(2_000), 1);
    assert!(shares.is_empty());
}

#[test]
fn prune_reports_every_removal() {
    // Five shares made at the same instant, so one instant retires all five: the count is the whole
    // store rather than a per-call unit. They differ only by the collision counter, because each call
    // is handed a fresh copy of the same entropy sequence.
    let mut shares = Shares::new();
    for _ in 0..5 {
        let _ = shares.create("session-a", 0, Some(10), &distinct());
    }
    assert_eq!(shares.len(), 5, "distinct tokens must all be kept");

    assert_eq!(shares.prune(10), 5);
    assert!(shares.is_empty());
}
