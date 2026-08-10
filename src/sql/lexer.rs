//! SQL tokenizer.
//!
//! Converts a SQL string into a stream of [`Token`]s for the recursive
//! descent parser in [`crate::sql::parser`].
//!
//! ## Keywords vs identifiers
//!
//! SQL keywords are matched case-insensitively (e.g. `select`, `Select`, and
//! `SELECT` all produce `Token::Keyword("SELECT")`). Identifiers preserve
//! their original case so column and table names round-trip correctly.
//!
//! ## Literals
//!
//! - **Integer**: a run of ASCII digits → `Token::Int(i64)`. Example: `42`.
//! - **Float**: digits containing a `.` or an exponent → `Token::Float(f64)`.
//!   Examples: `3.14`, `1e10`, `2.5E-3`.
//! - **String**: single-quoted, with `''` as the escape for a literal `'`.
//!   Example: `'hello'`, `'it''s'`.
//! - **Hex**: `x'...'` or `X'...'` with an even number of hex digits →
//!   `Token::Hex(Vec<u8>)`. Example: `x'0123'` → `vec![0x01, 0x23]`.

/// The set of reserved SQL keywords.
///
/// Stored as a single `&[&str]` slice so the tokenizer can do a linear scan;
/// the set is small enough (~40 entries) that a `match` would be only
/// marginally faster and considerably less readable.
pub const KEYWORDS: &[&str] = &[
    "SELECT",
    "FROM",
    "WHERE",
    "GROUP",
    "BY",
    "ORDER",
    "JOIN",
    "ON",
    "LIKE",
    "DATE",
    "TIMESTAMP",
    "INTERVAL",
    "EXTRACT",
    "CASE",
    "WHEN",
    "THEN",
    "ELSE",
    "END",
    "DISTINCT",
    "INSERT",
    "UPDATE",
    "DELETE",
    "BEGIN",
    "TRANSACTION",
    "COMMIT",
    "AND",
    "OR",
    "NOT",
    "AS",
    "APPROXIMATE",
    "WITHIN",
    "CONFIDENCE",
    "TIER",
    "SIMILAR",
    "TO",
    "HAMMING",
    "DISTANCE",
    "CONSISTENCY",
    "SCOPE",
    "RACK",
    "GLOBAL",
    "ASYNC",
    "USING",
    "MEMORY",
    "BUDGET",
    "ENERGY",
    "JOULES",
    "CONTINUOUS",
    "HAVING",
    "OUTER",
    "LEFT",
    "INNER",
    "QUERY",
    // DDL keywords (Wave 3)
    "CREATE",
    "TABLE",
    "DROP",
    "IF",
    "EXISTS",
    "INT",
    "INTEGER",
    "BIGINT",
    "SMALLINT",
    "TINYINT",
    "VARCHAR",
    "NVARCHAR",
    "TEXT",
    "FLOAT",
    "REAL",
    "DECIMAL",
    "NUMERIC",
    "BIT",
    "BOOLEAN",
    // Native type keywords (Wave 70)
    "JSON",
    "UUID",
    "BYTEA",
    "ENUM",
    "NULL",
    "DEFAULT",
    "PRIMARY",
    "KEY",
    "REFERENCES",
    "IDENTITY",
    "NOT",
    "SCHEMA",
    "ALTER",
    "ROLLBACK",
    // Wave 66: ALTER TABLE / CREATE INDEX keywords.
    "ADD",
    "COLUMN",
    "TYPE",
    "INDEX",
    // Wave 67: EXTRACT + CAST in basic parser.
    "CAST",
    "YEAR",
    "MONTH",
    "DAY",
    "HOUR",
    "MINUTE",
    "SECOND",
    // DML keywords (Wave 4)
    "VALUES",
    "SET",
    "INTO",
    "OUTPUT",
    // CTE keywords (Wave 6)
    "WITH",
    "UNION",
    "RECURSIVE",
    "ALL",
    "OPTION",
    "MAXRECURSION",
    // Window function keywords (Wave 7)
    "OVER",
    "PARTITION",
    "ROWS",
    "RANGE",
    "PRECEDING",
    "FOLLOWING",
    "CURRENT",
    "ROW",
    "NUMBER",
    // ClickBench parser fixes (Wave 16)
    "BETWEEN",
    "IN",
    "IS",
];

/// A single SQL token.
///
/// `PartialEq` is derived for unit-test assertions. Note that
/// `Token::Float(f64)` derives `PartialEq` from `f64`, which means `NaN !=
/// NaN`; this never arises in practice because the tokenizer never produces
/// `NaN` tokens.
#[derive(Debug, Clone, PartialEq)]
pub enum Token {
    /// A reserved keyword (uppercased). Example: `SELECT`, `FROM`.
    Keyword(String),
    /// An identifier (original case preserved). Example: `users`, `myColumn`.
    Ident(String),
    /// An integer literal.
    Int(i64),
    /// A floating-point literal.
    Float(f64),
    /// A single-quoted string literal (with `''` escapes already expanded).
    String(String),
    /// A hex literal `x'...'` decoded into raw bytes.
    Hex(Vec<u8>),
    /// A non-punctuation operator: `=`, `!=`, `<`, `>`, `<=`, `>=`, `+`, `-`,
    /// `*`, `/`.
    Op(String),
    /// Left parenthesis `(`.
    LParen,
    /// Right parenthesis `)`.
    RParen,
    /// Comma `,`.
    Comma,
    /// Semicolon `;`.
    Semicolon,
    /// End of input. Always the last token in the stream returned by
    /// [`tokenize`].
    EOF,
}

/// Tokenize a SQL string.
///
/// Returns the token stream with a trailing [`Token::EOF`]. Whitespace is
/// discarded; comments are not yet supported (a future wave may add `--`
/// line comments and `/* */` block comments).
///
/// # Errors
///
/// Returns `Err` with a human-readable message for:
/// - unterminated string literal (`'abc`)
/// - unterminated hex literal (`x'abc`)
/// - hex literal with an odd number of digits (`x'abc'`)
/// - invalid hex character (`x'gh'`)
/// - `!` not followed by `=` (lone `!`)
/// - any other unexpected character.
pub fn tokenize(input: &str) -> Result<Vec<Token>, String> {
    let mut tokens = Vec::new();
    let mut chars = input.chars().peekable();

    while let Some(&c) = chars.peek() {
        match c {
            // Whitespace.
            ' ' | '\t' | '\n' | '\r' => {
                chars.next();
            }
            // Punctuation.
            '(' => {
                chars.next();
                tokens.push(Token::LParen);
            }
            ')' => {
                chars.next();
                tokens.push(Token::RParen);
            }
            ',' => {
                chars.next();
                tokens.push(Token::Comma);
            }
            ';' => {
                chars.next();
                tokens.push(Token::Semicolon);
            }
            // Operators.
            '=' => {
                chars.next();
                tokens.push(Token::Op("=".to_string()));
            }
            '!' => {
                chars.next();
                if chars.peek() == Some(&'=') {
                    chars.next();
                    tokens.push(Token::Op("!=".to_string()));
                } else {
                    return Err(
                        "expected '!=' but found '!' followed by another character".to_string()
                    );
                }
            }
            '<' => {
                chars.next();
                if chars.peek() == Some(&'=') {
                    chars.next();
                    tokens.push(Token::Op("<=".to_string()));
                } else if chars.peek() == Some(&'>') {
                    chars.next();
                    tokens.push(Token::Op("<>".to_string()));
                } else {
                    tokens.push(Token::Op("<".to_string()));
                }
            }
            '>' => {
                chars.next();
                if chars.peek() == Some(&'=') {
                    chars.next();
                    tokens.push(Token::Op(">=".to_string()));
                } else {
                    tokens.push(Token::Op(">".to_string()));
                }
            }
            '+' | '-' | '*' | '/' => {
                chars.next();
                tokens.push(Token::Op(c.to_string()));
            }
            // String literal.
            '\'' => {
                chars.next();
                let s = read_string_literal(&mut chars)?;
                tokens.push(Token::String(s));
            }
            // Number or hex literal.
            '0'..='9' => {
                let (tok, _consumed) = read_number(&mut chars)?;
                tokens.push(tok);
            }
            // Float with no integer part (`.5`), or qualified-name
            // separator (`table.col`). A `.` followed by a letter
            // or underscore is treated as an operator so the parser can
            // build qualified column references.
            '.' => {
                let mut peek_iter = chars.clone();
                peek_iter.next(); // consume '.'
                match peek_iter.peek() {
                    Some(c) if c.is_ascii_digit() => {
                        let (tok, _consumed) = read_number(&mut chars)?;
                        tokens.push(tok);
                    }
                    _ => {
                        chars.next(); // consume '.'
                        tokens.push(Token::Op(".".to_string()));
                    }
                }
            }
            // Hex literal `x'...'` or `X'...'`, or an identifier starting
            // with x/X.
            'x' | 'X' => {
                // Peek the char after the x/X: if it's a quote, this is a
                // hex literal. Otherwise it's a regular identifier.
                let mut peek_iter = chars.clone();
                peek_iter.next(); // consume x/X
                if peek_iter.peek() == Some(&'\'') {
                    chars.next(); // consume x
                    chars.next(); // consume '
                    let bytes = read_hex_literal(&mut chars)?;
                    tokens.push(Token::Hex(bytes));
                } else {
                    let s = read_identifier(&mut chars);
                    push_word(&mut tokens, s);
                }
            }
            // Identifier or keyword.
            'a'..='z' | 'A'..='Z' | '_' => {
                let s = read_identifier(&mut chars);
                push_word(&mut tokens, s);
            }
            // Anything else is an error.
            _ => {
                return Err(format!("unexpected character: {c:?}"));
            }
        }
    }

    tokens.push(Token::EOF);
    Ok(tokens)
}

/// Read a run of identifier characters (alphanumeric + underscore). The
/// first character is assumed to already be a letter or underscore (the
/// caller checks).
fn read_identifier(chars: &mut std::iter::Peekable<std::str::Chars<'_>>) -> String {
    let mut s = String::new();
    while let Some(&c) = chars.peek() {
        if c.is_alphanumeric() || c == '_' {
            s.push(c);
            chars.next();
        } else {
            break;
        }
    }
    s
}

/// Push a word (identifier or keyword) onto the token stream. Keywords are
/// uppercased; identifiers preserve their original case.
fn push_word(tokens: &mut Vec<Token>, word: String) {
    let upper = word.to_uppercase();
    if KEYWORDS.contains(&upper.as_str()) {
        tokens.push(Token::Keyword(upper));
    } else {
        tokens.push(Token::Ident(word));
    }
}

/// Read a single-quoted string literal starting *after* the opening quote.
/// Handles the `''` escape (a literal single quote). Consumes the closing
/// quote. Returns `Err` if the string is unterminated.
fn read_string_literal(
    chars: &mut std::iter::Peekable<std::str::Chars<'_>>,
) -> Result<String, String> {
    let mut s = String::new();
    let mut closed = false;
    while let Some(&c) = chars.peek() {
        chars.next();
        if c == '\'' {
            // Check for escaped quote ''.
            if chars.peek() == Some(&'\'') {
                chars.next();
                s.push('\'');
            } else {
                closed = true;
                break;
            }
        } else {
            s.push(c);
        }
    }
    if !closed {
        return Err("unterminated string literal".to_string());
    }
    Ok(s)
}

/// Read a hex literal body (between the quotes) and decode it to bytes.
/// The opening `x'` (or `X'`) is assumed to already be consumed; this
/// function consumes the closing `'`. Returns `Err` on bad characters or
/// an odd digit count.
fn read_hex_literal(
    chars: &mut std::iter::Peekable<std::str::Chars<'_>>,
) -> Result<Vec<u8>, String> {
    let mut hex_str = String::new();
    let mut closed = false;
    while let Some(&c) = chars.peek() {
        if c == '\'' {
            chars.next();
            closed = true;
            break;
        }
        if c.is_ascii_hexdigit() {
            hex_str.push(c);
            chars.next();
        } else {
            return Err(format!("invalid hex character: {c:?}"));
        }
    }
    if !closed {
        return Err("unterminated hex literal".to_string());
    }
    if !hex_str.len().is_multiple_of(2) {
        return Err(format!(
            "hex literal must have an even number of digits, got {}",
            hex_str.len()
        ));
    }
    let bytes: Vec<u8> = (0..hex_str.len())
        .step_by(2)
        .map(|i| u8::from_str_radix(&hex_str[i..i + 2], 16).expect("validated as hex above"))
        .collect();
    Ok(bytes)
}

/// Read a numeric literal (integer or float). Returns the token and the
/// number of characters consumed (the count is unused at the call site but
/// kept for future diagnostics).
///
/// A number is a float if it contains a `.` or an exponent (`e`/`E`).
fn read_number(
    chars: &mut std::iter::Peekable<std::str::Chars<'_>>,
) -> Result<(Token, usize), String> {
    let mut s = String::new();
    let mut is_float = false;

    // Integer part (and fractional part if a `.` is present).
    while let Some(&c) = chars.peek() {
        if c.is_ascii_digit() {
            s.push(c);
            chars.next();
        } else if c == '.' {
            is_float = true;
            s.push(c);
            chars.next();
        } else {
            break;
        }
    }

    // Exponent.
    if chars.peek() == Some(&'e') || chars.peek() == Some(&'E') {
        is_float = true;
        s.push(chars.next().unwrap());
        if chars.peek() == Some(&'+') || chars.peek() == Some(&'-') {
            s.push(chars.next().unwrap());
        }
        while let Some(&c) = chars.peek() {
            if c.is_ascii_digit() {
                s.push(c);
                chars.next();
            } else {
                break;
            }
        }
    }

    let consumed = s.chars().count();
    let tok = if is_float {
        let f: f64 = s
            .parse()
            .map_err(|e: std::num::ParseFloatError| format!("invalid float {s:?}: {e}"))?;
        Token::Float(f)
    } else {
        let i: i64 = s
            .parse()
            .map_err(|e: std::num::ParseIntError| format!("invalid integer {s:?}: {e}"))?;
        Token::Int(i)
    };
    Ok((tok, consumed))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Helper: assert two token slices are equal, with `Token::Float`
    /// compared with a small epsilon.
    fn assert_tokens_eq(actual: &[Token], expected: &[Token]) {
        assert_eq!(actual.len(), expected.len(), "token count mismatch");
        for (i, (a, e)) in actual.iter().zip(expected.iter()).enumerate() {
            match (a, e) {
                (Token::Float(a), Token::Float(e)) => {
                    assert!((a - e).abs() < 1e-12, "token {i}: float {a} != {e}");
                }
                _ => assert_eq!(a, e, "token {i}: {a:?} != {e:?}"),
            }
        }
    }

    #[test]
    fn tokenize_simple_select() {
        let toks = tokenize("SELECT * FROM users WHERE id = 42").unwrap();
        assert_tokens_eq(
            &toks,
            &[
                Token::Keyword("SELECT".into()),
                Token::Op("*".into()),
                Token::Keyword("FROM".into()),
                Token::Ident("users".into()),
                Token::Keyword("WHERE".into()),
                Token::Ident("id".into()),
                Token::Op("=".into()),
                Token::Int(42),
                Token::EOF,
            ],
        );
    }

    #[test]
    fn tokenize_approximate_query() {
        let toks = tokenize("SELECT AVG(price) APPROXIMATE WITHIN 0.01 CONFIDENCE 0.99 FROM sales")
            .unwrap();
        assert_tokens_eq(
            &toks,
            &[
                Token::Keyword("SELECT".into()),
                Token::Ident("AVG".into()),
                Token::LParen,
                Token::Ident("price".into()),
                Token::RParen,
                Token::Keyword("APPROXIMATE".into()),
                Token::Keyword("WITHIN".into()),
                Token::Float(0.01),
                Token::Keyword("CONFIDENCE".into()),
                Token::Float(0.99),
                Token::Keyword("FROM".into()),
                Token::Ident("sales".into()),
                Token::EOF,
            ],
        );
    }

    #[test]
    fn tokenize_keywords_case_insensitive() {
        let toks = tokenize("select from where").unwrap();
        assert_tokens_eq(
            &toks,
            &[
                Token::Keyword("SELECT".into()),
                Token::Keyword("FROM".into()),
                Token::Keyword("WHERE".into()),
                Token::EOF,
            ],
        );
    }

    #[test]
    fn tokenize_identifiers_preserve_case() {
        let toks = tokenize("myColumn MY_other_col").unwrap();
        assert_tokens_eq(
            &toks,
            &[Token::Ident("myColumn".into()), Token::Ident("MY_other_col".into()), Token::EOF],
        );
    }

    #[test]
    fn tokenize_floats() {
        // `2.5` (not `3.14`) is used as the first float to avoid clippy's
        // `approx_constant` lint (3.14 ≈ π).
        let toks = tokenize("2.5 1e10 2.5E-3 .5").unwrap();
        assert_tokens_eq(
            &toks,
            &[
                Token::Float(2.5),
                Token::Float(1e10),
                Token::Float(2.5e-3),
                Token::Float(0.5),
                Token::EOF,
            ],
        );
    }

    #[test]
    fn tokenize_string_literals() {
        let toks = tokenize("'hello' 'it''s' ''").unwrap();
        assert_tokens_eq(
            &toks,
            &[
                Token::String("hello".into()),
                Token::String("it's".into()),
                Token::String("".into()),
                Token::EOF,
            ],
        );
    }

    #[test]
    fn tokenize_hex_literals() {
        let toks = tokenize("x'0123' X'AABB' x''").unwrap();
        assert_tokens_eq(
            &toks,
            &[
                Token::Hex(vec![0x01, 0x23]),
                Token::Hex(vec![0xAA, 0xBB]),
                Token::Hex(vec![]),
                Token::EOF,
            ],
        );
    }

    #[test]
    fn tokenize_all_operators() {
        let toks = tokenize("= != < > <= >= + - * /").unwrap();
        assert_tokens_eq(
            &toks,
            &[
                Token::Op("=".into()),
                Token::Op("!=".into()),
                Token::Op("<".into()),
                Token::Op(">".into()),
                Token::Op("<=".into()),
                Token::Op(">=".into()),
                Token::Op("+".into()),
                Token::Op("-".into()),
                Token::Op("*".into()),
                Token::Op("/".into()),
                Token::EOF,
            ],
        );
    }

    #[test]
    fn tokenize_punctuation() {
        let toks = tokenize("(a, b);").unwrap();
        assert_tokens_eq(
            &toks,
            &[
                Token::LParen,
                Token::Ident("a".into()),
                Token::Comma,
                Token::Ident("b".into()),
                Token::RParen,
                Token::Semicolon,
                Token::EOF,
            ],
        );
    }

    #[test]
    fn tokenize_x_identifier_not_hex() {
        // `x` followed by anything other than `'` is an identifier.
        let toks = tokenize("xray X1").unwrap();
        assert_tokens_eq(
            &toks,
            &[Token::Ident("xray".into()), Token::Ident("X1".into()), Token::EOF],
        );
    }

    #[test]
    fn tokenize_unterminated_string_errors() {
        assert!(tokenize("'abc").is_err());
    }

    #[test]
    fn tokenize_unterminated_hex_errors() {
        assert!(tokenize("x'abc").is_err());
    }

    #[test]
    fn tokenize_odd_hex_errors() {
        assert!(tokenize("x'abc'").is_err());
    }

    #[test]
    fn tokenize_bad_hex_char_errors() {
        assert!(tokenize("x'gh'").is_err());
    }

    #[test]
    fn tokenize_lone_bang_errors() {
        assert!(tokenize("!").is_err());
    }

    #[test]
    fn tokenize_unexpected_char_errors() {
        assert!(tokenize("@").is_err());
    }

    #[test]
    fn tokenize_negative_int() {
        // `-` is an operator, not part of the integer literal. The parser
        // composes them into a unary expression (or treats `-5` as `0 - 5`).
        let toks = tokenize("-5").unwrap();
        assert_tokens_eq(&toks, &[Token::Op("-".into()), Token::Int(5), Token::EOF]);
    }

    #[test]
    fn tokenize_empty_input() {
        let toks = tokenize("").unwrap();
        assert_tokens_eq(&toks, &[Token::EOF]);
    }

    #[test]
    fn tokenize_all_extension_keywords() {
        // Smoke-test: every keyword in KEYWORDS round-trips.
        let mut input = String::new();
        for kw in KEYWORDS {
            input.push_str(kw);
            input.push(' ');
        }
        let toks = tokenize(&input).unwrap();
        // Each keyword + EOF (trailing space is consumed as whitespace).
        assert_eq!(toks.len(), KEYWORDS.len() + 1);
        assert!(matches!(toks.last(), Some(Token::EOF)));
        for (i, kw) in KEYWORDS.iter().enumerate() {
            assert_eq!(toks[i], Token::Keyword((*kw).to_string()), "keyword {i}");
        }
    }
}
