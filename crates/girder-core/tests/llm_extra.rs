//! Tests for provider-neutral LLM failure classification.
//!
//! The reference vectors are copied from upstream, which is what makes this a port rather than an
//! interpretation:
//!
//! - `packages/llm/llm/tests/service.spec.ts` — the positive and, more importantly, the *negative*
//!   cases for the two prose classifiers. The negatives are the ones worth porting: they pin the
//!   narrowness that keeps an unrelated 400 from being reported as a context overrun.
//! - `packages/llm/llm/tests/retry-policy.spec.ts` — the default retryable set and the statement that
//!   the choice list is a superset of it.
//! - `packages/llm/llm-deepseek/src/transport.ts` — the status-to-code precedence, including the
//!   402-is-quota and 413-is-invalid-request rules.
//!
//! # Why the module is included by path
//!
//! `girder-core`'s crate root does not declare `llm_extra_gap`; adding `pub mod llm_extra_gap;` and its
//! `pub use` belongs to the integrator. The module is therefore compiled into this test binary
//! directly. It references `crate::RetryPolicy` in prose only, so it has no dependency on the rest of
//! the crate, and once the wiring lands the `#[path]` attribute should be replaced with
//! `use girder_core::llm::extra::*;`.

#[path = "../src/llm/extra.rs"]
pub mod llm_extra_gap;

use llm_extra_gap::{
    Classification, DEFAULT_RETRYABLE, EMPTY_RESPONSE, FAILURE_CODES, MAX_TIMER_DELAY_MS,
    ProviderError, QUOTA, RETRY_CODE_CHOICES, Signal, Violation, classify, classify_signal,
    is_context_window_exceeded, is_default_retryable, is_known, is_quota_exceeded, is_retry_choice,
    join_detail, known_codes, parse_retry_after,
};

/// A signal carrying only a message, with no status and a clock of zero.
fn message_only(message: &str) -> Signal<'_> {
    Signal {
        message,
        ..Signal::default()
    }
}

// ---------------------------------------------------------------------------------------------
// The upstream vectors, one assertion per line upstream writes them.
// ---------------------------------------------------------------------------------------------

#[test]
fn recognizes_structured_and_model_capacity_context_overflow() {
    // Upstream: `it('recognizes structured and model-capacity context-window overflow details')`.
    for detail in [
        "context_length_exceeded maximum context length",
        "context-window-overflowed",
        "This model maximum context length is 128000 tokens",
        "input is too long for this model",
        "request too large for model context",
        "input exceeds the model context window limit",
    ] {
        assert!(
            is_context_window_exceeded(detail),
            "expected context overflow for {detail:?}"
        );
    }
}

#[test]
fn does_not_mistake_input_validation_for_context_overflow() {
    // Upstream: `it('does not mistake unrelated input validation for context-window overflow')`.
    // These four are the reason every pattern is written narrowly; a classifier that matched any
    // "exceeds maximum" would fail here and silently compact a request that had a parameter bug.
    for detail in [
        "invalid request: malformed tool arguments",
        "invalid input: temperature exceeds maximum allowed value",
        "input exceeds maximum allowed value",
        "context window size must be positive",
    ] {
        assert!(
            !is_context_window_exceeded(detail),
            "expected no context overflow for {detail:?}"
        );
    }
}

#[test]
fn distinguishes_exhausted_quota_from_transient_rate_limiting() {
    // Upstream: `it('distinguishes exhausted account quota from transient rate limiting')`.
    for detail in [
        "insufficient_quota",
        "account balance depleted",
        "usage-limit-exceeded",
        "out of credits",
        "OpenAI API error (429): You exceeded your current quota, please check your plan and billing details.",
    ] {
        assert!(
            is_quota_exceeded(detail),
            "expected quota exhaustion for {detail:?}"
        );
    }
    for detail in ["HTTP 429: rate limit reached", "quota resets in one minute"] {
        assert!(
            !is_quota_exceeded(detail),
            "expected no quota exhaustion for {detail:?}"
        );
    }
}

// ---------------------------------------------------------------------------------------------
// Narrowness, stated as properties rather than as the same four examples again.
// ---------------------------------------------------------------------------------------------

#[test]
fn the_two_conditions_are_independent_but_never_both_required() {
    // A detail that names both is reported as both; `classify_signal` then applies precedence. Keeping
    // the flags independent is what lets the precedence live in one place instead of being duplicated
    // into the two classifiers.
    let both = classify("insufficient_quota: input exceeds the model context window limit");
    assert_eq!(
        both,
        Classification {
            context_window_exceeded: true,
            quota_exceeded: true,
        }
    );
    assert!(both.any());

    let neither = classify("HTTP 500: internal server error");
    assert_eq!(neither, Classification::default());
    assert!(!neither.any());
}

#[test]
fn a_context_word_far_from_the_predicate_is_not_an_overflow() {
    // The `.{0,40}` bound exists for this shape: a message that mentions both, but far apart, is
    // describing two things and not one overrun.
    let distant = format!(
        "input exceeds maximum allowed value; {}context window documented separately",
        "x ".repeat(30)
    );
    assert!(!is_context_window_exceeded(&distant));
}

#[test]
fn a_separator_run_is_not_a_word_gap_for_the_structured_pattern() {
    // `context__length` is one separator run in the quota patterns but not the single separator the
    // structured pattern allows. Upstream writes `[\s_-]` there, so this must not match.
    assert!(!is_context_window_exceeded("context__length__exceeded"));
    assert!(is_context_window_exceeded("context_length_exceeded"));
    assert!(is_context_window_exceeded("context-length-exceeded"));
    assert!(is_context_window_exceeded("context window exceeded"));
}

#[test]
fn a_leap_second_and_a_qualified_maximum_both_match() {
    // `max allowed context length` is the optional-qualifier branch, which is easy to write as
    // required-by-accident.
    assert!(is_context_window_exceeded(
        "max allowed context length reached"
    ));
    assert!(is_context_window_exceeded(
        "max supported context window reached"
    ));
    // The predicate's `s`/`ed` suffixes are separate alternatives upstream, not an `s?`.
    assert!(is_context_window_exceeded("context_length_exceeds"));
    assert!(is_context_window_exceeded("context_window_overflow"));
}

#[test]
fn quota_wording_requires_a_terminal_predicate() {
    // Every one of these names a quota without saying it is used up. Reporting them as exhausted would
    // stop a session that could have continued.
    for detail in [
        "quota management documentation",
        "your quota is 1000 requests per day",
        "context window exceeded", // a different code entirely
        "insufficient funds notice",
    ] {
        assert!(
            !is_quota_exceeded(detail),
            "unexpected quota for {detail:?}"
        );
    }
    // And the same words with a terminal predicate do match, including the `your`/`the` and `current`
    // qualifiers upstream makes optional.
    assert!(is_quota_exceeded("you exceeded your quota"));
    assert!(is_quota_exceeded("credits depleted"));
    assert!(is_quota_exceeded("out of budget"));
    // But the passive voice is not matched, in either build. Upstream's two quota patterns put the
    // predicate adjacent to the noun (`quota exceeded`) or before it (`exceeded your quota`), and
    // `quota was exceeded` has a copula in between, which neither allows. Recording it as false is the
    // honest port: quietly accepting it here would make this build classify errors upstream routes to
    // `UNKNOWN`, and the difference would surface only as a divergence in production.
    assert!(!is_quota_exceeded("the current quota was exceeded"));
}

// ---------------------------------------------------------------------------------------------
// Status-to-code precedence.
// ---------------------------------------------------------------------------------------------

#[test]
fn authentication_outranks_every_other_signal() {
    // A 401 whose body mentions a quota is still an auth failure: telling the user to top up an account
    // whose key is wrong wastes their time.
    let error = classify_signal(&Signal {
        status: Some(401),
        message: "insufficient_quota",
        ..Signal::default()
    });
    assert_eq!(error.code, llm_extra_gap::AUTH);

    let by_type = classify_signal(&Signal {
        kind: "permission_error",
        message: "",
        ..Signal::default()
    });
    assert_eq!(by_type.code, llm_extra_gap::AUTH);
}

#[test]
fn quota_outranks_the_rate_limit_that_delivers_it() {
    // The failure this module exists to prevent: an exhausted balance arrives as 429, and a scan for
    // "429" calls it retryable. Five backoffs against a balance that will not refill.
    let error = classify_signal(&Signal {
        status: Some(429),
        message: "You exceeded your current quota, please check your plan and billing details.",
        ..Signal::default()
    });
    assert_eq!(error.code, QUOTA);
    assert!(!is_default_retryable(&error.code));

    // A 402 is a quota even with no wording at all.
    let payment = classify_signal(&Signal {
        status: Some(402),
        ..Signal::default()
    });
    assert_eq!(payment.code, QUOTA);
}

#[test]
fn rate_limit_outranks_a_context_overrun_it_mentions() {
    // A 429 on a context overrun is the provider asking the caller to slow down; backoff is correct
    // even though the body mentions length.
    let error = classify_signal(&Signal {
        status: Some(429),
        message: "input exceeds the model context window limit",
        ..Signal::default()
    });
    assert_eq!(error.code, llm_extra_gap::RATE_LIMIT);
    assert!(is_default_retryable(&error.code));
}

#[test]
fn context_overflow_is_classified_from_prose_with_no_usable_status() {
    // Both a 400 and an in-band stream error reach here, and the second has no status at all.
    let bad_request = classify_signal(&Signal {
        status: Some(400),
        message: "This model's maximum context length is 128000 tokens",
        ..Signal::default()
    });
    assert_eq!(bad_request.code, llm_extra_gap::CONTEXT_WINDOW_EXCEEDED);
    assert!(is_known(&bad_request.code));
    assert!(!is_default_retryable(&bad_request.code));

    let in_band = classify_signal(&message_only(
        "input exceeds the model context window limit",
    ));
    assert_eq!(in_band.code, llm_extra_gap::CONTEXT_WINDOW_EXCEEDED);
    assert_eq!(in_band.status, None);
}

#[test]
fn an_unclaimed_status_keeps_its_number() {
    // `HTTP_<number>` is a code that cannot be enumerated, which is why `code` is a `String`. Flattening
    // it into SERVER would lose the only routing evidence the response carried.
    let error = classify_signal(&Signal {
        status: Some(418),
        message: "teapot",
        ..Signal::default()
    });
    assert_eq!(error.code, "HTTP_418");
    assert_eq!(error.status, Some(418));
    // Known to the vocabulary? No — and that is a legal answer.
    assert!(!is_known(&error.code));
    assert!(!is_retry_choice(&error.code));

    // 400 and 413 are claimed by INVALID_REQUEST, so neither becomes an HTTP_ code.
    for status in [400, 413] {
        let error = classify_signal(&Signal {
            status: Some(status),
            ..Signal::default()
        });
        assert_eq!(error.code, llm_extra_gap::INVALID_REQUEST);
    }
}

#[test]
fn a_server_failure_is_reached_by_status_or_by_type() {
    for status in [500, 502, 503, 504] {
        let error = classify_signal(&Signal {
            status: Some(status),
            ..Signal::default()
        });
        assert_eq!(error.code, llm_extra_gap::SERVER, "status {status}");
        assert!(is_default_retryable(&error.code));
    }
    let by_type = classify_signal(&Signal {
        kind: "overloaded_error",
        ..Signal::default()
    });
    assert_eq!(by_type.code, llm_extra_gap::SERVER);
    // No status and no recognized type is still a server failure rather than an unclassified one.
    assert_eq!(
        classify_signal(&message_only("boom")).code,
        llm_extra_gap::SERVER
    );
}

// ---------------------------------------------------------------------------------------------
// Serialized failure facts.
// ---------------------------------------------------------------------------------------------

#[test]
fn a_failure_renders_a_message_even_when_the_provider_sent_none() {
    let with_status = classify_signal(&Signal {
        status: Some(500),
        ..Signal::default()
    });
    assert_eq!(with_status.message, "provider request failed (HTTP 500)");

    let without_status = classify_signal(&Signal::default());
    assert_eq!(without_status.message, "provider request failed");
    assert!(!without_status.message.is_empty());
}

#[test]
fn the_provider_message_is_preserved_verbatim() {
    // A reader diagnosing a failure needs the provider's own words, not a rendering of them.
    let error = classify_signal(&Signal {
        status: Some(400),
        message: "invalid_request_error: messages: text content blocks must be non-empty",
        ..Signal::default()
    });
    assert_eq!(
        error.message,
        "invalid_request_error: messages: text content blocks must be non-empty"
    );
}

#[test]
fn a_failure_fact_is_validated_before_it_becomes_durable() {
    assert_eq!(ProviderError::new("SERVER", "boom").validate(), Ok(()));
    assert_eq!(
        ProviderError::new("SERVER", "").validate(),
        Err(Violation::EmptyMessage)
    );
    assert_eq!(
        ProviderError::new("", "boom").validate(),
        Err(Violation::EmptyCode)
    );

    let with = |mutate: fn(&mut ProviderError)| {
        let mut error = ProviderError::new("SERVER", "boom");
        mutate(&mut error);
        error.validate()
    };
    // A status outside the three-digit range is not a response code.
    assert_eq!(
        with(|error| error.status = Some(99)),
        Err(Violation::StatusOutOfRange)
    );
    assert_eq!(
        with(|error| error.status = Some(600)),
        Err(Violation::StatusOutOfRange)
    );
    // A zero retry delay is not a delay; it must be absent or positive.
    assert_eq!(
        with(|error| error.provider_retry_after_ms = Some(0)),
        Err(Violation::NonPositiveRetryAfter)
    );
    assert_eq!(
        with(|error| error.request_id = Some(String::new())),
        Err(Violation::EmptyRequestId)
    );
    assert_eq!(
        with(|error| error.offload_images = Some(0)),
        Err(Violation::NonPositiveOffload)
    );
    // All boundary values that are legal stay legal.
    assert_eq!(
        with(|error| {
            error.status = Some(599);
            error.provider_retry_after_ms = Some(1);
            error.request_id = Some("req-1".to_owned());
            error.offload_images = Some(1);
        }),
        Ok(())
    );
}

#[test]
fn image_offload_carries_its_own_code_and_count() {
    let mut error = ProviderError::new(
        llm_extra_gap::IMAGE_OFFLOAD_REQUIRED,
        "offload 2 more images",
    );
    error.offload_images = Some(2);
    assert_eq!(error.validate(), Ok(()));
    assert!(is_known(&error.code));
}

// ---------------------------------------------------------------------------------------------
// retry-after parsing: both wire forms, and the values that are dropped.
// ---------------------------------------------------------------------------------------------

#[test]
fn delta_seconds_retry_after_is_read_as_milliseconds() {
    assert_eq!(parse_retry_after("120", 0), Some(120_000));
    assert_eq!(parse_retry_after("0.5", 0), Some(500));
    assert_eq!(parse_retry_after("1.25", 0), Some(1_250));
    assert_eq!(parse_retry_after("  7 ", 0), Some(7_000));
    // Digits past the third are truncated, never rounded up: sleeping longer than the provider asked
    // for is the one direction that can be wrong.
    assert_eq!(parse_retry_after("0.001", 0), Some(1));
    assert_eq!(parse_retry_after("2.9999", 0), Some(2_999));
    // A fraction that truncates to zero is a zero delay, which is dropped rather than returned as `Some(0)`.
    assert_eq!(parse_retry_after("0.0009", 0), None);
}

#[test]
fn an_http_date_retry_after_is_the_distance_to_now() {
    // The RFC's own example instant is 784_111_777 seconds since the epoch.
    const EXAMPLE: u64 = 784_111_777_000;
    assert_eq!(
        parse_retry_after("Sun, 06 Nov 1994 08:49:37 GMT", EXAMPLE),
        None
    );
    assert_eq!(
        parse_retry_after("Sun, 06 Nov 1994 08:49:37 GMT", EXAMPLE - 5_000),
        Some(5_000)
    );
    // The weekday is decorative: it is not needed to compute an instant, so it is not required.
    assert_eq!(
        parse_retry_after("06 Nov 1994 08:49:37 GMT", EXAMPLE - 1_000),
        Some(1_000)
    );
    // A date already in the past is not a delay.
    assert_eq!(
        parse_retry_after("Sun, 06 Nov 1994 08:49:37 GMT", EXAMPLE + 1_000),
        None
    );
}

#[test]
fn http_date_calendar_arithmetic_is_exact_across_eras() {
    // Checked against the epoch itself, the day before it, a leap day, and a date past 2038 — the
    // cases an implementation that forgot the era shift, the leap rule, or the sign would fail.
    assert_eq!(
        parse_retry_after("Thu, 01 Jan 1970 00:00:00 GMT", 0),
        None // the delay is zero, which is not a usable instruction
    );
    assert_eq!(
        parse_retry_after("Fri, 02 Jan 1970 00:00:00 GMT", 0),
        Some(86_400_000)
    );
    assert_eq!(
        parse_retry_after("Wed, 31 Dec 1969 23:59:59 GMT", 0),
        None // before the epoch: `u64` milliseconds cannot represent it
    );
    // 2000-02-29 exists (a leap year under the 400 rule); 1900-02-29 does not.
    assert_eq!(
        parse_retry_after("Tue, 29 Feb 2000 00:00:00 GMT", 951_782_400_000),
        None
    );
    assert_eq!(parse_retry_after("Thu, 29 Feb 1900 00:00:00 GMT", 0), None);
    // A far-future instant is representable and its delay is reported.
    let far = parse_retry_after("Fri, 01 Jan 2100 00:00:00 GMT", 4_102_444_800_000 - 60_000);
    assert_eq!(far, Some(60_000));
}

#[test]
fn the_epoch_is_a_thursday_and_the_arithmetic_agrees() {
    // The module exposes no date formatter, so this pins `days_from_civil` against the epoch by way of
    // the one date every implementation is checked against.
    assert_eq!(
        parse_retry_after("Sat, 03 Jan 1970 00:00:00 GMT", 0),
        Some(2 * 86_400_000)
    );
}

#[test]
fn an_unusable_retry_after_is_dropped_rather_than_approximated() {
    for value in [
        "",
        "   ",
        "soon",
        "0",
        "-5",
        "1.2.3",
        "5.",
        "abc123",
        "Sun, 06 Nov 1994 08:49:37 PST", // wrong zone
        "Sun, 06 Nov 1994 08:49:37",     // no zone at all
        "Sun, 31 Feb 1994 08:49:37 GMT", // no such day
        "Sun, 06 Nov 1994 24:49:37 GMT", // no such hour
        "Sun, 06 Nov 1994 08:60:37 GMT", // no such minute
        "Sun, 06 Nov 1994 08:49 GMT",    // no seconds
        "Sun, 06 Nov 1994 08:49:37 GMT extra",
    ] {
        assert_eq!(parse_retry_after(value, 0), None, "value {value:?}");
    }
}

#[test]
fn a_retry_after_beyond_the_timer_ceiling_is_dropped() {
    // A provider asking for a week is asking for a wait that outlives the session; the caller's own
    // backoff is the answer, not a clamped approximation of the provider's number.
    assert_eq!(
        parse_retry_after(&(MAX_TIMER_DELAY_MS / 1_000).to_string(), 0),
        Some(MAX_TIMER_DELAY_MS / 1_000 * 1_000)
    );
    assert_eq!(
        parse_retry_after(&(MAX_TIMER_DELAY_MS / 1_000 + 1).to_string(), 0),
        None
    );
}

#[test]
fn the_signal_resolves_retry_after_and_request_id_onto_the_failure() {
    let error = classify_signal(&Signal {
        status: Some(503),
        message: "overloaded",
        retry_after: Some("3"),
        request_id: Some("req-abc"),
        now_unix_ms: 0,
        ..Signal::default()
    });
    assert_eq!(error.code, llm_extra_gap::SERVER);
    assert_eq!(error.provider_retry_after_ms, Some(3_000));
    assert_eq!(error.request_id.as_deref(), Some("req-abc"));
    assert_eq!(error.validate(), Ok(()));

    // An empty request id is an absent one, not an empty identity.
    let empty_id = classify_signal(&Signal {
        request_id: Some(""),
        ..Signal::default()
    });
    assert_eq!(empty_id.request_id, None);
    assert_eq!(empty_id.validate(), Ok(()));
}

// ---------------------------------------------------------------------------------------------
// The vocabulary itself.
// ---------------------------------------------------------------------------------------------

#[test]
fn the_code_vocabulary_matches_upstreams_key_set() {
    // 52 entries, which is `Object.keys(LLM_FAILURE_CODES).length` upstream. Counting is the only way
    // to notice a code that was dropped while porting, since nothing would reference it.
    assert_eq!(FAILURE_CODES.len(), 52);
    assert_eq!(known_codes().len(), 52);

    // Sorted and duplicate-free, so a lookup cannot depend on position.
    let mut sorted = FAILURE_CODES;
    sorted.sort_unstable();
    assert_eq!(sorted, FAILURE_CODES);
    let unique: std::collections::BTreeSet<_> = FAILURE_CODES.into_iter().collect();
    assert_eq!(unique.len(), FAILURE_CODES.len());

    // Every constant this module names is a member of its own vocabulary.
    for code in [
        llm_extra_gap::CONTEXT_WINDOW_EXCEEDED,
        QUOTA,
        llm_extra_gap::ACCOUNT_QUOTA,
        EMPTY_RESPONSE,
        llm_extra_gap::INVALID_CREDENTIAL,
        llm_extra_gap::AUTH,
        llm_extra_gap::INVALID_REQUEST,
        llm_extra_gap::SERVER,
        llm_extra_gap::RATE_LIMIT,
        llm_extra_gap::STREAM_CLOSED,
        llm_extra_gap::MALFORMED_RESPONSE,
        llm_extra_gap::TRANSPORT,
        llm_extra_gap::TIMEOUT,
        llm_extra_gap::NO_ADAPTER,
        llm_extra_gap::IMAGE_OFFLOAD_REQUIRED,
        llm_extra_gap::UNKNOWN,
    ] {
        assert!(is_known(code), "{code} is not in FAILURE_CODES");
    }
}

#[test]
fn the_retry_choice_list_is_a_strict_superset_of_the_default_retryable_set() {
    // Upstream states both facts and relies on the difference: `QUOTA` is selectable but not selected.
    assert_eq!(DEFAULT_RETRYABLE.len(), 5);
    assert_eq!(RETRY_CODE_CHOICES.len(), 8);
    for code in DEFAULT_RETRYABLE {
        assert!(is_retry_choice(code), "{code} must be offerable");
        assert!(is_default_retryable(code));
    }
    for code in ["MALFORMED_RESPONSE", "QUOTA", "STREAM_CLOSED"] {
        assert!(is_retry_choice(code), "{code} must be offerable");
        assert!(!is_default_retryable(code), "{code} must not be default");
    }
    // The default set is exactly the intersection the retry policy in `retry` uses.
    assert_eq!(
        DEFAULT_RETRYABLE,
        [
            EMPTY_RESPONSE,
            llm_extra_gap::RATE_LIMIT,
            llm_extra_gap::SERVER,
            llm_extra_gap::TIMEOUT,
            llm_extra_gap::TRANSPORT,
        ]
    );
}

#[test]
fn context_overflow_and_quota_are_never_retried_by_default() {
    // The two codes whose whole purpose is to stop a retry loop from waiting for a change that cannot
    // come.
    for code in [
        llm_extra_gap::CONTEXT_WINDOW_EXCEEDED,
        QUOTA,
        llm_extra_gap::ACCOUNT_QUOTA,
        llm_extra_gap::AUTH,
        llm_extra_gap::INVALID_REQUEST,
        llm_extra_gap::INVALID_CREDENTIAL,
        llm_extra_gap::ABORTED,
    ] {
        assert!(!is_default_retryable(code), "{code} must not be retried");
    }
}

#[test]
fn join_detail_orders_code_before_message_and_skips_empties() {
    // The order is part of the contract: a structured code comes first so a classifier never has to
    // depend on the message's phrasing.
    assert_eq!(
        join_detail(
            "invalid_request_error",
            "context_length_exceeded",
            "too long"
        ),
        "invalid_request_error context_length_exceeded too long"
    );
    assert_eq!(
        join_detail("", "context_length_exceeded", ""),
        "context_length_exceeded"
    );
    assert_eq!(join_detail("", "", ""), "");
    assert_eq!(join_detail("", "", "too long"), "too long");
}

#[test]
fn joining_then_classifying_finds_a_code_the_message_does_not_mention() {
    // OpenAI sends `code: context_length_exceeded` with a message that says "too long"; a classifier
    // that saw only the message would miss the clearest evidence in the payload.
    let detail = join_detail(
        "invalid_request_error",
        "context_length_exceeded",
        "too long",
    );
    assert!(is_context_window_exceeded(&detail));
}


// Path aliases so a file compiled with #[path] still resolves crate::<domain>::<module>.
mod llm {
    pub use super::llm_extra_gap as extra;
}
