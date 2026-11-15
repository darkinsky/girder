//! A small boolean query syntax for text search.
//!
//! [`Query::text`](crate::session_query::Query) holds one case-insensitive substring. That is enough
//! for "does this session mention X" and nothing else: it cannot express "mentions X but not Y", and
//! it cannot say "either X or Y". The gap is one users hit immediately — the session they are looking
//! for is the one where `error` appears and `warning` does not, and today the only way to find it is
//! to run two searches and subtract the result sets by hand.
//!
//! This module is the missing layer: it parses a query string into a small tree and evaluates that
//! tree against one piece of text. It deliberately does *not* know about sessions, events, or filters.
//! A caller keeps walking the catalogue exactly as before and swaps `text.contains(word)` for
//! [`matches`], so the syntax costs no new I/O and can be adopted one call site at a time.
//!
//! # Why a syntax here, when a struct was chosen elsewhere
//!
//! [`crate::session_filter`] argues against a query string: its caller is a program, so a bad request
//! should be a type error at the caller rather than a parse error at the server. That reasoning does
//! not transfer to text search, and the difference is who writes the query. A structural filter is
//! composed by code from fields it already has; a text query is typed by a person into a box, one
//! keystroke at a time, and [a struct literal is not something a person types]. The parse error is
//! therefore not a defect to avoid — it is the feedback the person needs, which is why [`QueryError`]
//! carries the byte offset of the problem rather than a bare "invalid query".
//!
//! [a struct literal is not something a person types]: https://en.wikipedia.org/wiki/Principle_of_least_astonishment
//!
//! # The syntax
//!
//! ```text
//! word            contains "word", case-insensitively
//! -word           does not contain "word"
//! a b             both, i.e. a AND b written without the keyword
//! a OR b          either
//! "a b"           the phrase "a b", spaces included
//! ( ... )         grouping, to override precedence
//! ```
//!
//! Precedence, loosest first: `OR`, then implicit AND, then `-`, then parentheses. So
//! `a b OR c` is `(a AND b) OR c`, which is what the reading order suggests.
//!
//! # The four decisions that matter
//!
//! **Implicit AND.** `a b` means both, because that is what people type. Requiring the word `AND`
//! would make the common query the verbose one, and every search box that has ever required it has
//! taught users to type a phrase into a substring matcher instead.
//!
//! **`-` for NOT.** It is the `grep -v` reflex, and it is a prefix rather than an infix operator so
//! that negation attaches to one term without parentheses.
//!
//! **Quotes for phrases.** Without them, a phrase is indistinguishable from two terms, and a user
//! searching `"no such file"` wants the phrase, not a session that says "no" somewhere and "such"
//! elsewhere. A quoted phrase is a single term rather than a sequence, so it participates in `-` and
//! in grouping like any other term.
//!
//! **Case-insensitive.** Search is almost always meant that way. A user searching `ERROR` wants the
//! `error` line too, and a case-sensitive default mainly produces "no results" for a session that is
//! plainly there.
//!
//! # Why a hand-written parser rather than regular expressions
//!
//! The grammar nests: parentheses contain expressions that contain parentheses. A regular expression
//! cannot describe that without a recursion workaround, and the usual workaround — matching balanced
//! delimiters with a pattern — is unreadable at the point of definition and cannot report *where* the
//! input stopped making sense. A recursive-descent parser is one function per precedence level, each
//! five to ten lines, and every error carries the token that caused it.
//!
//! # What this module does not do
//!
//! There is no `AND` keyword, no field syntax (`title:foo`), no wildcard, no fuzzy match, and no
//! regular expression. Each is a real feature and each has a cost — `AND` needs a lookahead rule,
//! fields need a schema, regex needs a dependency and a pathologically-slow-pattern defence. The
//! grammar above is what a user needs to narrow a search to one session, and nothing here forecloses
//! adding the rest later: they are all new tokens, not a new shape.

/// A query expression.
///
/// Terms hold the text as the user wrote it; matching lowercases both sides, so the spelling in the
/// tree stays useful for echoing the query back to the user. The tree is not simplified — `--a` stays
/// two `Not` nodes — because a caller that displays the parsed query is better served by what was
/// written than by a normal form it cannot explain.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Expr {
    /// Contains this text, ignoring case.
    Term(String),
    /// Does not hold.
    Not(Box<Expr>),
    /// Both hold.
    And(Box<Expr>, Box<Expr>),
    /// Either holds.
    Or(Box<Expr>, Box<Expr>),
    /// Holds of any text.
    ///
    /// Produced by an empty group `()`. It exists so that a caller assembling a query from optional
    /// clauses has an identity to fall back on instead of special-casing "no clauses" at every level.
    All,
}

/// Why a query string could not be parsed.
///
/// Every variant carries a byte offset into the input so a caller can point at the offending
/// character. A byte offset rather than a character offset because that is what the caller needs to
/// slice the input, and a caret positioned by character would still have to convert.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum QueryError {
    /// A `(` was never closed, or a `)` had no opening `(`.
    ///
    /// The offset is the position of the unmatched parenthesis itself, not the end of the input: when
    /// a group is unclosed, the opening bracket is the token the reader has to fix.
    #[error("unbalanced parenthesis at byte {at}")]
    UnbalancedParen {
        /// Byte offset of the unmatched parenthesis.
        at: usize,
    },

    /// A `"` was never closed.
    #[error("unclosed quotation mark at byte {at}")]
    UnclosedQuote {
        /// Byte offset of the opening quote.
        at: usize,
    },

    /// The input held no terms at all.
    ///
    /// An empty string, or only whitespace. Distinct from a parse failure because the caller's
    /// response differs: an empty box is not an error to report, it is a search that has not been
    /// typed yet. It is still not [`Expr::All`] — a caller that wants every match can say so with
    /// `()` — because a blank box silently matching everything is how a filter box appears broken.
    #[error("the query is empty")]
    Empty,

    /// A token appeared where the expression could not continue: an operand was expected and the
    /// input ended, or an operator had nothing to operate on.
    ///
    /// `a OR` reports the offset just past `OR`, and a lone `-` reports the offset of the `-`.
    #[error("unexpected end of query at byte {at}")]
    Trailing {
        /// Byte offset where the operand was expected.
        at: usize,
    },
}

/// Parses a query string into an expression.
///
/// The grammar, loosest binding first:
///
/// ```text
/// or      := and ( "OR" and )*
/// and     := unary ( unary )*        -- juxtaposition is AND
/// unary   := "-" unary | primary
/// primary := "(" or ")" | term | phrase
/// ```
///
/// `()` parses to [`Expr::All`], the "constrains nothing" identity. Behaviour worth knowing:
///
/// - Only an uppercase `OR` is an operator. Lowercase `or` is a word you can search for, which
///   matters because it is one of the most common words in English prose — the same rule every other
///   search box uses, and for the same reason.
/// - `-` negates only at the start of a token, so `girder-core` and `e-mail` stay single terms.
///   A literal leading hyphen, as in `--verbose`, must be quoted: `"-v"`. Unquoted `--verbose` reads
///   as a double negation, which cancels — consistent, though rarely what is meant.
/// - Matching is substring-based, not word-boundary-based, which is what
///   [`Query::text`](crate::session_query::Query) already does. `error` therefore matches
///   `errors` and `stderr`. Refusing that now would silently narrow every existing caller.
/// - No escapes exist inside a phrase, so a phrase cannot contain `"`. Adding an escape syntax would
///   make `\` meaningful in every query; the ability to search for a quote character is worth less
///   than the ability to type a Windows path.
///
/// # Errors
///
/// [`QueryError::Empty`] for blank input, [`QueryError::UnclosedQuote`] for an unterminated phrase,
/// [`QueryError::UnbalancedParen`] for parentheses that do not pair, and [`QueryError::Trailing`] for
/// an operator with no operand.
pub fn parse(text: &str) -> Result<Expr, QueryError> {
    let tokens = tokenize(text)?;
    if tokens.is_empty() {
        return Err(QueryError::Empty);
    }
    let mut parser = Parser {
        tokens,
        next: 0,
        end: text.len(),
    };
    let expression = parser.parse_or()?;
    // Anything left over is a token the grammar could not attach: a stray `)` is a bracket problem,
    // and anything else (a second `OR`, say) is an operator with nothing to join.
    match parser.tokens.get(parser.next) {
        None => Ok(expression),
        Some(spanned) => Err(match spanned.token {
            Token::RParen => QueryError::UnbalancedParen { at: spanned.at },
            Token::LParen | Token::Or | Token::Not | Token::Term(_) => {
                QueryError::Trailing { at: spanned.at }
            }
        }),
    }
}

/// Whether `text` satisfies `expression`.
///
/// Both sides are lowercased, so matching ignores case in the Unicode sense rather than the ASCII one
/// — `STRAßE` and `straße` agree. The haystack is lowered once per call by [`matches`] and reused
/// across the tree, which is why the recursion takes it as an argument instead of re-lowering at
/// every `And` branch.
///
/// An empty term — `""`, or a phrase of only spaces — matches everything, because every string does
/// contain the empty string. That is the mathematical answer and it is also the safe one: a query
/// that accidentally lost its content matches broadly rather than silently returning nothing.
#[must_use]
pub fn matches(expression: &Expr, text: &str) -> bool {
    evaluate(expression, &text.to_lowercase())
}

/// Evaluates against an already-lowercased haystack.
fn evaluate(expression: &Expr, haystack: &str) -> bool {
    match expression {
        Expr::Term(term) => haystack.contains(&term.to_lowercase()),
        Expr::Not(inner) => !evaluate(inner, haystack),
        // `&&` and `||` short-circuit, so a cheap left branch can spare an expensive right one.
        Expr::And(left, right) => evaluate(left, haystack) && evaluate(right, haystack),
        Expr::Or(left, right) => evaluate(left, haystack) || evaluate(right, haystack),
        Expr::All => true,
    }
}

/// One lexical unit, before it has a place in the tree.
#[derive(Clone, Debug, PartialEq, Eq)]
enum Token {
    /// `(`
    LParen,
    /// `)`
    RParen,
    /// The `OR` keyword.
    Or,
    /// A leading `-`.
    Not,
    /// A bare word or a quoted phrase; the payload is the text itself.
    Term(String),
}

/// A token and the byte offset it started at, so errors can point at it.
#[derive(Clone, Debug, PartialEq, Eq)]
struct Spanned {
    /// The token.
    token: Token,
    /// Its byte offset in the original input.
    at: usize,
}

/// Splits the input into tokens.
///
/// Kept separate from the parser so that "the input is not even tokenizable" (an unterminated quote)
/// is reported before any structural error, and so the parser can look ahead without re-scanning text.
fn tokenize(text: &str) -> Result<Vec<Spanned>, QueryError> {
    let mut tokens = Vec::new();
    let mut chars = text.char_indices().peekable();

    while let Some(&(at, current)) = chars.peek() {
        if current.is_whitespace() {
            chars.next();
            continue;
        }
        match current {
            '(' => {
                chars.next();
                tokens.push(Spanned {
                    token: Token::LParen,
                    at,
                });
            }
            ')' => {
                chars.next();
                tokens.push(Spanned {
                    token: Token::RParen,
                    at,
                });
            }
            // Only at a token boundary, which this position is by construction: the word scanner
            // below consumes hyphens that appear inside a word.
            '-' => {
                chars.next();
                tokens.push(Spanned {
                    token: Token::Not,
                    at,
                });
            }
            '"' => {
                chars.next();
                let mut phrase = String::new();
                let mut closed = false;
                for (_, inner) in chars.by_ref() {
                    if inner == '"' {
                        closed = true;
                        break;
                    }
                    phrase.push(inner);
                }
                if !closed {
                    return Err(QueryError::UnclosedQuote { at });
                }
                tokens.push(Spanned {
                    token: Token::Term(phrase),
                    at,
                });
            }
            _ => {
                let mut word = String::new();
                while let Some(&(_, inner)) = chars.peek() {
                    // Hyphens are *not* delimiters: `girder-core` is one term. Negation is a token
                    // prefix, which the arm above enforces by firing only at a boundary.
                    if inner.is_whitespace() || matches!(inner, '(' | ')' | '"') {
                        break;
                    }
                    word.push(inner);
                    chars.next();
                }
                // Uppercase only: `or` stays searchable. See the module documentation.
                if word == "OR" {
                    tokens.push(Spanned {
                        token: Token::Or,
                        at,
                    });
                } else {
                    tokens.push(Spanned {
                        token: Token::Term(word),
                        at,
                    });
                }
            }
        }
    }

    Ok(tokens)
}

/// One token of lookahead over a finished token list, one method per precedence level.
struct Parser {
    /// The tokens to consume.
    tokens: Vec<Spanned>,
    /// Index of the next unconsumed token.
    next: usize,
    /// Byte length of the input, used to point at "the end" when an operand is missing.
    end: usize,
}

impl Parser {
    /// The next token, if any.
    fn peek(&self) -> Option<&Spanned> {
        self.tokens.get(self.next)
    }

    /// `or := and ( "OR" and )*`
    fn parse_or(&mut self) -> Result<Expr, QueryError> {
        let mut left = self.parse_and()?;
        while matches!(self.peek().map(|spanned| &spanned.token), Some(Token::Or)) {
            self.next += 1;
            let right = self.parse_and()?;
            left = Expr::Or(Box::new(left), Box::new(right));
        }
        Ok(left)
    }

    /// `and := unary ( unary )*`, the second form being the implicit AND.
    ///
    /// The loop stops at `)`, at `OR`, and at the end. Those are exactly the positions where a
    /// following token belongs to an enclosing rule rather than to this one.
    fn parse_and(&mut self) -> Result<Expr, QueryError> {
        let mut left = self.parse_unary()?;
        loop {
            match self.peek() {
                None | Some(Spanned {
                    token: Token::RParen | Token::Or,
                    ..
                }) => return Ok(left),
                Some(_) => {
                    let right = self.parse_unary()?;
                    left = Expr::And(Box::new(left), Box::new(right));
                }
            }
        }
    }

    /// `unary := "-" unary | primary`
    ///
    /// Right-recursive so `--a` parses as `Not(Not(a))` rather than as an error. Negating twice is
    /// odd but unambiguous, and rejecting it would mean `--` had to be a special case.
    fn parse_unary(&mut self) -> Result<Expr, QueryError> {
        if let Some(Spanned {
            token: Token::Not,
            at,
        }) = self.peek()
        {
            let at = *at;
            self.next += 1;
            if self.peek().is_none() {
                // A trailing `-` negates nothing. Reported as `Trailing` rather than a dedicated
                // variant: the reader's mistake is the same one as `a OR` — an operator with no
                // operand — and a caller shows them the same way.
                return Err(QueryError::Trailing { at });
            }
            let inner = self.parse_unary()?;
            return Ok(Expr::Not(Box::new(inner)));
        }
        self.parse_primary()
    }

    /// `primary := "(" or ")" | term | phrase`
    fn parse_primary(&mut self) -> Result<Expr, QueryError> {
        let Some(spanned) = self.tokens.get(self.next) else {
            return Err(QueryError::Trailing { at: self.end });
        };
        let at = spanned.at;
        let token = spanned.token.clone();
        self.next += 1;
        match token {
            Token::Term(term) => Ok(Expr::Term(term)),
            Token::LParen => {
                // `()` is the identity, not an error: it is what a query builder produces when every
                // optional clause was omitted, and "no constraint" is exactly what that means.
                if matches!(self.peek().map(|spanned| &spanned.token), Some(Token::RParen)) {
                    self.next += 1;
                    return Ok(Expr::All);
                }
                let inner = self.parse_or()?;
                if matches!(self.peek().map(|spanned| &spanned.token), Some(Token::RParen)) {
                    self.next += 1;
                    return Ok(inner);
                }
                Err(QueryError::UnbalancedParen { at })
            }
            Token::RParen => Err(QueryError::UnbalancedParen { at }),
            // Defensive: `parse_unary` consumes every `-`, and a complete expression cannot be
            // followed by an `OR` without it being seen there. Returning an error rather than
            // panicking keeps a caller-supplied query from aborting the process.
            Token::Or | Token::Not => Err(QueryError::Trailing { at }),
        }
    }
}
