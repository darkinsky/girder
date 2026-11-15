//! Tests for the boolean query syntax.
//!
//! Three properties carry the weight, and each is a decision the module documentation argues for:
//! juxtaposition means AND (the common query is the short one), a leading `-` negates (the `grep -v`
//! reflex), and a quoted phrase is one term rather than two. The error cases get equal weight because
//! the input is typed by a person: an unclosed quote or an unmatched bracket must be reported with a
//! position, not silently accepted, since a parser that guesses would search for something the user
//! did not ask for.
//!
//! The module is included by path rather than through the crate root, so these tests compile against
//! it as written even before the crate root wires it in.

#[path = "../src/query_syntax.rs"]
mod query_syntax;

use query_syntax::{Expr, QueryError, matches, parse};

/// Parses a query that is expected to be valid.
fn ok(text: &str) -> Expr {
    parse(text).expect("the query should parse")
}

/// Whether `query` matches `text`, for the common one-liner.
fn hit(query: &str, text: &str) -> bool {
    matches(&ok(query), text)
}

#[test]
fn a_bare_word_matches_a_substring() {
    assert!(hit("error", "the build failed with an error"));
    // Substring rather than whole word: that is what `Query::text` already does, and narrowing it
    // here would silently change the meaning of every existing caller.
    assert!(hit("error", "two errors appeared"));
    assert!(hit("error", "stderr shows an error"));
    assert!(!hit("error", "the build succeeded"));
}

#[test]
fn matching_ignores_case_in_both_directions() {
    // Both directions matter: lowercasing only the haystack was a real bug in the sibling module,
    // and it is invisible when the test only ever types lowercase.
    assert!(hit("error", "An ERROR appeared"));
    assert!(hit("ERROR", "an error appeared"));
    assert!(hit("ErRoR", "An ErRoR appeared"));
    // A term with no case at all must still work.
    assert!(hit("42", "exit code 42"));
}

#[test]
fn juxtaposition_means_and() {
    // The core of the syntax: the common query must be the short one.
    assert!(hit("error build", "the build reported an error"));
    assert!(!hit("error build", "the build succeeded"));
    assert!(!hit("error build", "an error while testing"));
    // Order does not matter, because AND is commutative and users do not remember word order.
    assert!(hit("build error", "an error during the build"));
}

#[test]
fn three_terms_are_anded_left_to_right() {
    assert!(hit("a b c", "c b a"));
    assert!(!hit("a b c", "a b"));
}

#[test]
fn a_leading_hyphen_excludes() {
    // The `grep -v` intuition, which is the reason `-` is a prefix rather than an infix operator.
    assert!(hit("error -warning", "one error, no complaints"));
    assert!(!hit("error -warning", "one error and one warning"));
    // A negation on its own is a complete query and matches everything the word is absent from.
    assert!(hit("-error", "all is well"));
    assert!(!hit("-error", "an error"));
}

#[test]
fn negation_can_be_nested() {
    // `--a` is odd but unambiguous. Rejecting it would make `-` need a special case.
    assert!(hit("--error", "an error"));
    assert!(!hit("--error", "all is well"));
}

#[test]
fn a_hyphen_inside_a_word_is_literal() {
    // Negation is a token prefix, so a hyphen after the first character must not split the word.
    // Without this, searching for a crate name would silently become a `-v` query.
    assert!(hit("girder-core", "see girder-core for details"));
    assert!(!hit("girder-core", "see girder for details"));
    assert!(hit("-girder-core", "see girder for details"));
}

#[test]
fn the_or_keyword_matches_either_side() {
    assert!(hit("error OR warning", "a warning appeared"));
    assert!(hit("error OR warning", "an error appeared"));
    assert!(!hit("error OR warning", "everything was fine"));
    // Either side of an `OR` may itself be a conjunction.
    assert!(hit("build error OR warning", "a warning during a build"));
    assert!(!hit("build error OR warning", "a test during a build"));
}

#[test]
fn lowercase_or_is_a_searchable_word() {
    // Only uppercase `OR` is an operator, so the common English word stays searchable. This is the
    // rule every search box uses, and it is why the keyword is not case-insensitive.
    assert!(hit("or", "this or that"));
    assert!(hit("either or", "either or both"));
}

#[test]
fn or_binds_looser_than_implicit_and() {
    // `a b OR c` must read as `(a AND b) OR c`, which is what the word order suggests.
    assert!(hit("a b OR c", "c alone"));
    assert!(!hit("a b OR c", "a alone"));
    assert!(hit("a b OR c", "a and b"));
}

#[test]
fn a_quoted_phrase_is_one_term() {
    // The whole point of quotes: the three words must be adjacent and in order.
    assert!(hit("\"no such file\"", "the shell said no such file"));
    assert!(!hit("\"no such file\"", "no problem, such a file exists"));
    // Without quotes the same text is three terms, which matches a different set of inputs.
    assert!(hit("no such file", "no problem, such a file exists"));
}

#[test]
fn a_phrase_participates_in_boolean_operators() {
    assert!(hit("\"file not found\" -panic", "file not found, aborting"));
    assert!(!hit("\"file not found\" -panic", "file not found, panic"));
    assert!(hit("\"a b\" OR \"c d\"", "it was c d"));
}

#[test]
fn parentheses_override_precedence() {
    // `a AND (b OR c)` and `(a AND b) OR c` are different queries; the brackets are the only way to
    // say the first.
    assert!(hit("a (b OR c)", "a and c"));
    // "cc" rather than a word containing `c` by accident: the other clause is the bare term `a`,
    // which would match any haystack that happens to contain that letter on its own.
    assert!(!hit("a (b OR c)", "cc"));
    assert!(hit("(a OR b) c", "b and c"));
    assert!(!hit("(a OR b) c", "bb"));
    // Nested groups.
    assert!(hit("((a OR b) (c OR d))", "b and d"));
    assert!(!hit("((a OR b) (c OR d))", "b alone"));
}

#[test]
fn a_negated_group_is_possible() {
    assert!(hit("-(error OR warning)", "all fine"));
    assert!(!hit("-(error OR warning)", "a warning"));
}

#[test]
fn an_empty_group_constrains_nothing() {
    // `()` is the identity a query builder falls back on when every optional clause is absent.
    assert_eq!(ok("()"), Expr::All);
    assert!(hit("()", "anything at all"));
    assert!(hit("()", ""));
    assert!(hit("(())", "anything at all"));
}

#[test]
fn extra_whitespace_is_ignored() {
    assert!(hit("  error   build  ", "an error during the build"));
    assert!(hit("\terror\n", "an error"));
    assert_eq!(ok("  error  "), Expr::Term("error".to_owned()));
}

#[test]
fn an_empty_phrase_matches_anything() {
    // Every string contains the empty string. The safe direction: a query that lost its content
    // matches broadly rather than silently returning nothing.
    assert!(hit("\"\"", "anything"));
    assert!(hit("\"\"", ""));
}

#[test]
fn an_empty_query_is_reported_as_empty() {
    assert_eq!(parse(""), Err(QueryError::Empty));
    assert_eq!(parse("   "), Err(QueryError::Empty));
    assert_eq!(parse("\n\t "), Err(QueryError::Empty));
}

#[test]
fn an_unclosed_quote_reports_its_position() {
    // The offset is the opening quote, which is the character the reader has to fix.
    assert_eq!(
        parse("\"no such file"),
        Err(QueryError::UnclosedQuote { at: 0 })
    );
    assert_eq!(
        parse("error \"build"),
        Err(QueryError::UnclosedQuote { at: 6 })
    );
    // A single `"` is unclosed, not an empty phrase — the two must not be confused.
    assert_eq!(parse("\""), Err(QueryError::UnclosedQuote { at: 0 }));
}

#[test]
fn an_unmatched_opening_paren_reports_its_position() {
    assert_eq!(parse("(a b"), Err(QueryError::UnbalancedParen { at: 0 }));
    assert_eq!(
        parse("a (b OR c"),
        Err(QueryError::UnbalancedParen { at: 2 })
    );
    // The innermost unclosed bracket is the one reported, because it is the nearest fix.
    assert_eq!(parse("((a)"), Err(QueryError::UnbalancedParen { at: 0 }));
}

#[test]
fn an_unmatched_closing_paren_reports_its_position() {
    assert_eq!(parse("a b)"), Err(QueryError::UnbalancedParen { at: 3 }));
    assert_eq!(parse(")"), Err(QueryError::UnbalancedParen { at: 0 }));
    assert_eq!(parse("(a))"), Err(QueryError::UnbalancedParen { at: 3 }));
}

#[test]
fn a_dangling_operator_reports_where_the_operand_was_expected() {
    // An `OR` with nothing after it, and a `-` with nothing after it, are the same mistake: an
    // operator with no operand.
    assert_eq!(parse("a OR"), Err(QueryError::Trailing { at: 4 }));
    assert_eq!(parse("-"), Err(QueryError::Trailing { at: 0 }));
    assert_eq!(parse("error -"), Err(QueryError::Trailing { at: 6 }));
    // An `OR` where a term belongs cannot start an expression.
    assert_eq!(parse("OR a"), Err(QueryError::Trailing { at: 0 }));
}

#[test]
fn parsing_is_deterministic_and_does_not_need_the_haystack() {
    // The tree is built from the query alone, so a caller can parse once and match many times.
    let expression = ok("error -warning");
    assert_eq!(
        expression,
        Expr::And(
            Box::new(Expr::Term("error".to_owned())),
            Box::new(Expr::Not(Box::new(Expr::Term("warning".to_owned())))),
        )
    );
    assert!(matches(&expression, "an error"));
    assert!(!matches(&expression, "an error and a warning"));
    // The same content in the other order: NOT applies wherever the term appears, not only at the
    // end, which is what a reader of `error -warning` expects.
    assert!(!matches(&expression, "a warning and an error"));
    // No `error` at all, so the conjunction fails however the `-warning` is placed: the assertion is about the whole
    // expression, not about the order the terms appeared in.
    assert!(!matches(&expression, "a warning about the build"));
    assert!(matches(&expression, "an error about the build"));
}

#[test]
fn the_parsed_tree_keeps_the_users_spelling() {
    // Case folding happens at match time, not parse time, so a caller echoing the query back to the
    // user shows what was typed.
    assert_eq!(ok("ERROR"), Expr::Term("ERROR".to_owned()));
    assert!(matches(&ok("ERROR"), "an error"));
}

#[test]
fn the_headline_query_works() {
    // The query the module exists for: sessions that mention an error but not a warning.
    let expression = ok("error -warning");
    assert!(matches(&expression, "thread panicked with an error"));
    assert!(!matches(
        &expression,
        "thread panicked with an error: warning"
    ));
    assert!(!matches(&expression, "everything is fine"));
}
