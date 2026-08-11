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
    // DML extension keywords (Wave 5) — RETURNING, CONFLICT, NOTHING,
    // DO used by INSERT/UPDATE/DELETE ... RETURNING and ON CONFLICT.
    "RETURNING",
    "CONFLICT",
    "NOTHING",
    "DO",
    // CTE keywords (Wave 6)
    "WITH",
    "UNION",
    "RECURSIVE",
    "ALL",
    "OPTION",
    "MAXRECURSION",
    // Set operation keywords (Wave 4) — INTERSECT and EXCEPT are required
    // for set operations to be recognised by the parser's match_keyword.
    "INTERSECT",
    "EXCEPT",
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
    /// A positional parameter placeholder (`$1`, `$2`, ...). The number
    /// is the 1-based parameter index.
    Param(u16),
    /// An anonymous parameter placeholder `?` (used by prepared-statement
    /// APIs that bind parameters positionally).
    QuestionMark,
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
            // Anonymous parameter placeholder `?`.
            '?' => {
                chars.next();
                tokens.push(Token::QuestionMark);
            }
            // Positional parameter placeholder `$1`, `$2`, ...
            '$' => {
                chars.next();
                let n = read_param_index(&mut chars)?;
                tokens.push(Token::Param(n));
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
            '+' => {
                chars.next();
                tokens.push(Token::Op("+".to_string()));
            }
            '-' => {
                chars.next();
                if chars.peek() == Some(&'-') {
                    // Line comment: `-- ...` skips until newline (or EOF).
                    chars.next(); // consume second '-'
                    skip_line_comment(&mut chars);
                } else if chars.peek() == Some(&'>') {
                    chars.next(); // consume '>'
                    if chars.peek() == Some(&'>') {
                        chars.next(); // consume second '>'
                        tokens.push(Token::Op("->>".to_string()));
                    } else {
                        tokens.push(Token::Op("->".to_string()));
                    }
                } else {
                    tokens.push(Token::Op("-".to_string()));
                }
            }
            '*' => {
                chars.next();
                tokens.push(Token::Op("*".to_string()));
            }
            '/' => {
                chars.next();
                if chars.peek() == Some(&'*') {
                    // Block comment: `/* ... */` with nesting support.
                    chars.next(); // consume '*'
                    skip_block_comment(&mut chars)?;
                } else {
                    tokens.push(Token::Op("/".to_string()));
                }
            }
            '%' => {
                chars.next();
                tokens.push(Token::Op("%".to_string()));
            }
            '|' => {
                chars.next();
                if chars.peek() == Some(&'|') {
                    chars.next();
                    tokens.push(Token::Op("||".to_string()));
                } else {
                    return Err("expected '||' but found '|' followed by another character".to_string());
                }
            }
            ':' => {
                chars.next();
                if chars.peek() == Some(&':') {
                    chars.next();
                    tokens.push(Token::Op("::".to_string()));
                } else {
                    return Err("expected '::' but found ':' followed by another character".to_string());
                }
            }
            // String literal.
            '\'' => {
                chars.next();
                let s = read_string_literal(&mut chars)?;
                tokens.push(Token::String(s));
            }
            // Double-quoted identifier. The contents are taken literally
            // (case preserved, spaces allowed) and never matched against
            // the keyword table — `"order"` is an identifier, not ORDER.
            // A doubled `""` inside the quotes is an escaped literal `"`.
            '"' => {
                chars.next();
                let s = read_quoted_identifier(&mut chars)?;
                tokens.push(Token::Ident(s));
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
            // Escape string literal `E'...'` or `e'...'`. Inside, C-style
            // backslash escapes (\n, \t, \\, \', etc.) are processed.
            'e' | 'E' => {
                let mut peek_iter = chars.clone();
                peek_iter.next(); // consume e/E
                if peek_iter.peek() == Some(&'\'') {
                    chars.next(); // consume e
                    chars.next(); // consume '
                    let s = read_escape_string_literal(&mut chars)?;
                    tokens.push(Token::String(s));
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

/// Read the digits of a positional parameter placeholder (`$1`, `$2`, ...).
/// The leading `$` is assumed to be already consumed. Returns `Err` if no
/// digits follow, or if the number overflows `u16`.
fn read_param_index(
    chars: &mut std::iter::Peekable<std::str::Chars<'_>>,
) -> Result<u16, String> {
    let mut s = String::new();
    while let Some(&c) = chars.peek() {
        if c.is_ascii_digit() {
            s.push(c);
            chars.next();
        } else {
            break;
        }
    }
    if s.is_empty() {
        return Err("expected digit after '$'".to_string());
    }
    let n: u16 = s
        .parse()
        .map_err(|e: std::num::ParseIntError| format!("invalid parameter index {s:?}: {e}"))?;
    Ok(n)
}

/// Skip the body of a line comment. The opening `--` is assumed to be
/// already consumed; this function reads characters until it hits a
/// newline (which is left in the stream so the caller's whitespace
/// branch can consume it) or end of input.
fn skip_line_comment(chars: &mut std::iter::Peekable<std::str::Chars<'_>>) {
    while let Some(&c) = chars.peek() {
        if c == '\n' {
            break;
        }
        chars.next();
    }
}

/// Skip the body of a block comment, supporting nesting. The opening
/// `/*` is assumed to be already consumed; this function consumes
/// characters until it reaches the matching `*/` (the depth counter
/// tracks nested `/* ... */` pairs). Returns `Err` if the comment is
/// unterminated.
fn skip_block_comment(
    chars: &mut std::iter::Peekable<std::str::Chars<'_>>,
) -> Result<(), String> {
    let mut depth: u32 = 1;
    while let Some(c) = chars.next() {
        if c == '/' && chars.peek() == Some(&'*') {
            chars.next(); // consume '*'
            depth = depth
                .checked_add(1)
                .ok_or_else(|| "block comment nesting too deep".to_string())?;
        } else if c == '*' && chars.peek() == Some(&'/') {
            chars.next(); // consume '/'
            depth -= 1;
            if depth == 0 {
                return Ok(());
            }
        }
    }
    Err("unterminated block comment".to_string())
}

/// Read an escape string literal body (the text after `E'`), processing
/// C-style backslash escapes. Consumes the closing `'`. Returns `Err` if
/// the string is unterminated or contains an unknown escape sequence.
///
/// Supported escapes (PostgreSQL `E'...'` syntax):
/// - `\b` → backspace (U+0008)
/// - `\f` → form feed (U+000C)
/// - `\n` → newline (U+000A)
/// - `\r` → carriage return (U+000D)
/// - `\t` → tab (U+0009)
/// - `\v` → vertical tab (U+000B)
/// - `\\` → backslash
/// - `\'` → single quote
/// - `\0` → NUL (U+0000)
/// - `\xHH` → single byte with hex value HH (two hex digits required)
/// - `\uXXXX` → Unicode code point (4 hex digits)
/// - `\UXXXXXXXX` → Unicode code point (8 hex digits)
/// - Any other `\c` is an error.
fn read_escape_string_literal(
    chars: &mut std::iter::Peekable<std::str::Chars<'_>>,
) -> Result<String, String> {
    let mut s = String::new();
    let mut closed = false;
    while let Some(&c) = chars.peek() {
        chars.next();
        if c == '\'' {
            // In escape strings, `''` is still a literal single quote.
            if chars.peek() == Some(&'\'') {
                chars.next();
                s.push('\'');
            } else {
                closed = true;
                break;
            }
        } else if c == '\\' {
            let escaped = chars
                .next()
                .ok_or_else(|| "unterminated escape string literal".to_string())?;
            match escaped {
                'b' => s.push('\u{0008}'),
                'f' => s.push('\u{000C}'),
                'n' => s.push('\n'),
                'r' => s.push('\r'),
                't' => s.push('\t'),
                'v' => s.push('\u{000B}'),
                '\\' => s.push('\\'),
                '\'' => s.push('\''),
                '0' => s.push('\u{0000}'),
                'x' => {
                    let b = read_hex_escape(chars, 2)?;
                    // \xHH is a single byte; only ASCII bytes map to a char.
                    let ch = u8::try_from(b)
                        .ok()
                        .and_then(|byte| byte.try_into().ok())
                        .ok_or_else(|| format!("invalid byte escape \\x{b:02X}"))?;
                    s.push(ch);
                }
                'u' => {
                    let b = read_hex_escape(chars, 4)?;
                    if let Some(ch) = char::from_u32(b) {
                        s.push(ch);
                    } else {
                        return Err(format!("invalid Unicode code point U+{b:04X}"));
                    }
                }
                'U' => {
                    let b = read_hex_escape(chars, 8)?;
                    if let Some(ch) = char::from_u32(b) {
                        s.push(ch);
                    } else {
                        return Err(format!("invalid Unicode code point U+{b:08X}"));
                    }
                }
                other => {
                    return Err(format!("unknown escape sequence \\{other}"));
                }
            }
        } else {
            s.push(c);
        }
    }
    if !closed {
        return Err("unterminated escape string literal".to_string());
    }
    Ok(s)
}

/// Read `n` hex digits and return the resulting `u32`. Used by escape
/// string literals for `\xHH`, `\uXXXX`, and `\UXXXXXXXX`.
fn read_hex_escape(
    chars: &mut std::iter::Peekable<std::str::Chars<'_>>,
    n: usize,
) -> Result<u32, String> {
    let mut value: u32 = 0;
    for _ in 0..n {
        let c = chars
            .next()
            .ok_or_else(|| "unterminated hex escape".to_string())?;
        let d = c
            .to_digit(16)
            .ok_or_else(|| format!("invalid hex digit in escape: {c:?}"))?;
        value = value
            .checked_mul(16)
            .and_then(|v| v.checked_add(d))
            .ok_or_else(|| "hex escape overflow".to_string())?;
    }
    Ok(value)
}

/// Read a double-quoted identifier body, starting *after* the opening `"`.
///
/// Doubled `""` inside the quotes is an escaped literal `"` (matching
/// PostgreSQL's behavior). Consumes the closing `"`. Returns `Err` if the
/// identifier is unterminated. Unlike plain identifiers, the contents are
/// not uppercased or matched against the keyword table.
fn read_quoted_identifier(
    chars: &mut std::iter::Peekable<std::str::Chars<'_>>,
) -> Result<String, String> {
    let mut s = String::new();
    let mut closed = false;
    while let Some(&c) = chars.peek() {
        chars.next();
        if c == '"' {
            if chars.peek() == Some(&'"') {
                chars.next();
                s.push('"');
            } else {
                closed = true;
                break;
            }
        } else {
            s.push(c);
        }
    }
    if !closed {
        return Err("unterminated quoted identifier".to_string());
    }
    Ok(s)
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
    fn tokenize_line_comment() {
        // `-- comment` to end of line is discarded; the trailing newline
        // becomes whitespace.
        let toks = tokenize("SELECT 1 -- comment\n").unwrap();
        assert_tokens_eq(
            &toks,
            &[
                Token::Keyword("SELECT".into()),
                Token::Int(1),
                Token::EOF,
            ],
        );
    }

    #[test]
    fn tokenize_line_comment_at_eof() {
        // No trailing newline: comment runs to EOF.
        let toks = tokenize("SELECT 1 -- no newline").unwrap();
        assert_tokens_eq(
            &toks,
            &[
                Token::Keyword("SELECT".into()),
                Token::Int(1),
                Token::EOF,
            ],
        );
    }

    #[test]
    fn tokenize_block_comment() {
        let toks = tokenize("SELECT /* block */ 1").unwrap();
        assert_tokens_eq(
            &toks,
            &[
                Token::Keyword("SELECT".into()),
                Token::Int(1),
                Token::EOF,
            ],
        );
    }

    #[test]
    fn tokenize_nested_block_comment() {
        // Nested block comments must be tracked by depth, not by string scan.
        let toks = tokenize("SELECT /* outer /* inner */ still outer */ 1").unwrap();
        assert_tokens_eq(
            &toks,
            &[
                Token::Keyword("SELECT".into()),
                Token::Int(1),
                Token::EOF,
            ],
        );
    }

    #[test]
    fn tokenize_block_comment_between_tokens() {
        let toks = tokenize("SELECT/*c*/1").unwrap();
        assert_tokens_eq(
            &toks,
            &[
                Token::Keyword("SELECT".into()),
                Token::Int(1),
                Token::EOF,
            ],
        );
    }

    #[test]
    fn tokenize_unterminated_block_comment_errors() {
        assert!(tokenize("SELECT /* never closed").is_err());
    }

    #[test]
    fn tokenize_block_comment_does_not_eat_division() {
        // `a / b` is division, not a comment.
        let toks = tokenize("a / b").unwrap();
        assert_tokens_eq(
            &toks,
            &[
                Token::Ident("a".into()),
                Token::Op("/".into()),
                Token::Ident("b".into()),
                Token::EOF,
            ],
        );
    }

    #[test]
    fn tokenize_quoted_identifier() {
        // Spaces and case are preserved inside double quotes.
        let toks = tokenize("SELECT \"my column\" FROM t").unwrap();
        assert_tokens_eq(
            &toks,
            &[
                Token::Keyword("SELECT".into()),
                Token::Ident("my column".into()),
                Token::Keyword("FROM".into()),
                Token::Ident("t".into()),
                Token::EOF,
            ],
        );
    }

    #[test]
    fn tokenize_quoted_identifier_preserves_case() {
        let toks = tokenize("\"MyCol\"").unwrap();
        assert_tokens_eq(&toks, &[Token::Ident("MyCol".into()), Token::EOF]);
    }

    #[test]
    fn tokenize_quoted_identifier_not_keyword() {
        // `"order"` is an identifier, not the keyword ORDER.
        let toks = tokenize("\"order\"").unwrap();
        assert_tokens_eq(&toks, &[Token::Ident("order".into()), Token::EOF]);
    }

    #[test]
    fn tokenize_quoted_identifier_escaped_quote() {
        // `""` inside the quotes is an escaped literal `"`.
        let toks = tokenize("\"a\"\"b\"").unwrap();
        assert_tokens_eq(&toks, &[Token::Ident("a\"b".into()), Token::EOF]);
    }

    #[test]
    fn tokenize_quoted_identifier_empty() {
        let toks = tokenize("\"\"").unwrap();
        assert_tokens_eq(&toks, &[Token::Ident("".into()), Token::EOF]);
    }

    #[test]
    fn tokenize_unterminated_quoted_identifier_errors() {
        assert!(tokenize("\"never closed").is_err());
    }

    #[test]
    fn tokenize_positional_param() {
        let toks = tokenize("SELECT $1, $2").unwrap();
        assert_tokens_eq(
            &toks,
            &[
                Token::Keyword("SELECT".into()),
                Token::Param(1),
                Token::Comma,
                Token::Param(2),
                Token::EOF,
            ],
        );
    }

    #[test]
    fn tokenize_anonymous_param() {
        let toks = tokenize("SELECT ?").unwrap();
        assert_tokens_eq(
            &toks,
            &[
                Token::Keyword("SELECT".into()),
                Token::QuestionMark,
                Token::EOF,
            ],
        );
    }

    #[test]
    fn tokenize_param_in_where() {
        let toks = tokenize("WHERE id = $1 AND name = ?").unwrap();
        assert_tokens_eq(
            &toks,
            &[
                Token::Keyword("WHERE".into()),
                Token::Ident("id".into()),
                Token::Op("=".into()),
                Token::Param(1),
                Token::Keyword("AND".into()),
                Token::Ident("name".into()),
                Token::Op("=".into()),
                Token::QuestionMark,
                Token::EOF,
            ],
        );
    }

    #[test]
    fn tokenize_param_large_index() {
        let toks = tokenize("$65535").unwrap();
        assert_tokens_eq(&toks, &[Token::Param(65535), Token::EOF]);
    }

    #[test]
    fn tokenize_param_missing_digit_errors() {
        assert!(tokenize("SELECT $").is_err());
        assert!(tokenize("SELECT $abc").is_err());
    }

    #[test]
    fn tokenize_concat_op() {
        let toks = tokenize("a || b").unwrap();
        assert_tokens_eq(
            &toks,
            &[
                Token::Ident("a".into()),
                Token::Op("||".into()),
                Token::Ident("b".into()),
                Token::EOF,
            ],
        );
    }

    #[test]
    fn tokenize_modulo_op() {
        let toks = tokenize("a % b").unwrap();
        assert_tokens_eq(
            &toks,
            &[
                Token::Ident("a".into()),
                Token::Op("%".into()),
                Token::Ident("b".into()),
                Token::EOF,
            ],
        );
    }

    #[test]
    fn tokenize_cast_op() {
        // `int` is a keyword; the cast operator splits identifier/keyword
        // from the type name. Use a non-keyword identifier for clarity.
        let toks = tokenize("a::int").unwrap();
        assert_tokens_eq(
            &toks,
            &[
                Token::Ident("a".into()),
                Token::Op("::".into()),
                Token::Keyword("INT".into()),
                Token::EOF,
            ],
        );
        let toks = tokenize("name::varchar").unwrap();
        assert_tokens_eq(
            &toks,
            &[
                Token::Ident("name".into()),
                Token::Op("::".into()),
                Token::Keyword("VARCHAR".into()),
                Token::EOF,
            ],
        );
    }

    #[test]
    fn tokenize_json_arrow_ops() {
        // `->` returns JSON; `->>` returns text.
        let toks = tokenize("j->'key'").unwrap();
        assert_tokens_eq(
            &toks,
            &[
                Token::Ident("j".into()),
                Token::Op("->".into()),
                Token::String("key".into()),
                Token::EOF,
            ],
        );
        let toks = tokenize("j->>'key'").unwrap();
        assert_tokens_eq(
            &toks,
            &[
                Token::Ident("j".into()),
                Token::Op("->>".into()),
                Token::String("key".into()),
                Token::EOF,
            ],
        );
    }

    #[test]
    fn tokenize_lone_pipe_errors() {
        assert!(tokenize("a | b").is_err());
    }

    #[test]
    fn tokenize_lone_colon_errors() {
        assert!(tokenize("a : b").is_err());
    }

    #[test]
    fn tokenize_arrow_at_eof() {
        let toks = tokenize("a->").unwrap();
        assert_tokens_eq(
            &toks,
            &[
                Token::Ident("a".into()),
                Token::Op("->".into()),
                Token::EOF,
            ],
        );
    }

    #[test]
    fn tokenize_escape_string_basic() {
        // Escape sequences are processed: \n becomes an actual newline.
        let toks = tokenize("E'hello\\n'").unwrap();
        assert_tokens_eq(&toks, &[Token::String("hello\n".into()), Token::EOF]);
    }

    #[test]
    fn tokenize_escape_string_lowercase_e() {
        let toks = tokenize("e'tab\\there'").unwrap();
        assert_tokens_eq(&toks, &[Token::String("tab\there".into()), Token::EOF]);
    }

    #[test]
    fn tokenize_escape_string_all_escapes() {
        let toks = tokenize("E'\\b\\f\\n\\r\\t\\v\\\\\\'\\0'").unwrap();
        let expected: String = [
            '\u{0008}', '\u{000C}', '\n', '\r', '\t', '\u{000B}', '\\', '\'', '\u{0000}',
        ]
        .iter()
        .collect();
        assert_tokens_eq(&toks, &[Token::String(expected), Token::EOF]);
    }

    #[test]
    fn tokenize_escape_string_hex_escape() {
        let toks = tokenize("E'\\x41\\x42'").unwrap();
        // 0x41 = 'A', 0x42 = 'B'
        assert_tokens_eq(&toks, &[Token::String("AB".into()), Token::EOF]);
    }

    #[test]
    fn tokenize_escape_string_unicode_escape() {
        let toks = tokenize("E'\\u00e9'").unwrap();
        // U+00E9 = é
        assert_tokens_eq(&toks, &[Token::String("é".into()), Token::EOF]);
    }

    #[test]
    fn tokenize_escape_string_doubled_quote() {
        // `''` inside an escape string is still a literal single quote.
        let toks = tokenize("E'a''b'").unwrap();
        assert_tokens_eq(&toks, &[Token::String("a'b".into()), Token::EOF]);
    }

    #[test]
    fn tokenize_escape_string_unknown_escape_errors() {
        assert!(tokenize("E'\\z'").is_err());
    }

    #[test]
    fn tokenize_escape_string_unterminated_errors() {
        assert!(tokenize("E'unterminated").is_err());
        assert!(tokenize("E'unterminated\\").is_err());
    }

    #[test]
    fn tokenize_escape_string_does_not_swallow_identifier() {
        // `email` is an identifier, not an escape string.
        let toks = tokenize("email = 'x'").unwrap();
        assert_tokens_eq(
            &toks,
            &[
                Token::Ident("email".into()),
                Token::Op("=".into()),
                Token::String("x".into()),
                Token::EOF,
            ],
        );
    }

    #[test]
    fn tokenize_typed_date_literal() {
        // `DATE '...'` is left as a keyword followed by a string; the
        // parser handles the typed-literal semantics.
        let toks = tokenize("DATE '2024-01-01'").unwrap();
        assert_tokens_eq(
            &toks,
            &[
                Token::Keyword("DATE".into()),
                Token::String("2024-01-01".into()),
                Token::EOF,
            ],
        );
    }

    #[test]
    fn tokenize_typed_timestamp_literal() {
        let toks = tokenize("TIMESTAMP '2024-01-01 12:00:00'").unwrap();
        assert_tokens_eq(
            &toks,
            &[
                Token::Keyword("TIMESTAMP".into()),
                Token::String("2024-01-01 12:00:00".into()),
                Token::EOF,
            ],
        );
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
