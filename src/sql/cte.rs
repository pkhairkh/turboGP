//! # CTE parser (Wave 7: token-based).
//!
//! Parses `WITH [RECURSIVE] name [(col1, col2, ...)] AS (body) [, ...] SELECT ...`.
//!
//! Wave 7 replaces the previous string-based scanner with a proper
//! token-based parser. The CTE body is captured as a token slice and
//! re-tokenised into a [`crate::sql::parser::SetQuery`] via
//! [`crate::sql::parser::parse_set`]. Recursive CTEs are detected by
//! token inspection (looking for a top-level `UNION ALL` inside the
//! body), not by string search — so string literals containing the
//! text "UNION ALL" no longer corrupt the parse.
//!
//! For backward compatibility with existing consumers, the `anchor`,
//! `recursive`, and `outer_query` fields are preserved as SQL strings
//! (re-serialised from the token slice).

use crate::sql::lexer::{tokenize, Token};
use crate::sql::parser::{parse_set, SetQuery};

/// One CTE definition: name + body (and optional recursive part).
#[derive(Debug, Clone)]
pub struct CteDef {
    /// The CTE name (e.g. "EmployeeHierarchy" in `WITH EmployeeHierarchy AS (...)`).
    pub name: String,
    /// Optional column list (e.g. `WITH t(a, b) AS (...)`). None means
    /// the CTE inherits its columns from the body's SELECT list.
    pub columns: Option<Vec<String>>,
    /// The anchor query (the non-recursive seed), as a SQL string.
    /// Re-serialised from the parsed token slice for backward compat.
    pub anchor: String,
    /// The recursive query (references the CTE name), as a SQL string.
    /// None for non-recursive CTEs. When present, the body contains a
    /// top-level UNION ALL.
    pub recursive: Option<String>,
    /// The parsed CTE body as a [`SetQuery`]. For non-recursive CTEs,
    /// this is a single `Select`. For recursive CTEs, this is a
    /// `UnionAll(anchor, recursive)`.
    pub body: Option<SetQuery>,
}

/// A parsed WITH clause: one or more CTEs plus the outer query.
#[derive(Debug, Clone)]
pub struct WithClause {
    /// The CTE definitions.
    pub ctes: Vec<CteDef>,
    /// The outer query (everything after the CTE definitions), as a SQL
    /// string. Re-serialised from the parsed token slice.
    pub outer_query: String,
    /// The parsed outer query as a [`SetQuery`]. None if parsing failed
    /// (in which case `outer_query` still holds the raw SQL).
    pub outer: Option<SetQuery>,
    /// MAXRECURSION hint (default 100, 0 = unlimited).
    pub max_recursion: u32,
}

/// Parse a WITH clause from a SQL string. Returns `None` if the string
/// doesn't start with WITH (so callers can fall through to other
/// parsers). Returns `Some(Ok(...))` on success, `Some(Err(...))` on
/// parse error.
///
/// # Errors
///
/// Returns `Err(String)` for malformed WITH clauses: missing AS,
/// unterminated parens, etc.
pub fn parse_with(sql: &str) -> Option<Result<WithClause, String>> {
    // Quick check: does the SQL start with WITH (after trimming whitespace)?
    let trimmed = sql.trim_start();
    if !trimmed.to_uppercase().starts_with("WITH ") && !trimmed.to_uppercase().starts_with("WITH\t")
    {
        return None;
    }
    Some(parse_with_inner(sql))
}

fn parse_with_inner(sql: &str) -> Result<WithClause, String> {
    // Tokenise the entire input. This is the key change from the old
    // string-based parser: we walk tokens, not characters.
    let tokens = tokenize(sql).map_err(|e| format!("CTE lex error: {e}"))?;

    let mut pos = 0;
    // Expect WITH
    expect_keyword(&tokens, &mut pos, "WITH")?;

    // Optional RECURSIVE
    let _recursive_keyword = match_keyword(&tokens, pos, "RECURSIVE");
    if _recursive_keyword {
        pos += 1;
    }

    // Parse CTE definitions: name [(cols)] AS (body), ...
    let mut ctes = Vec::new();
    loop {
        // CTE name
        let name = expect_ident(&tokens, &mut pos)?;

        // Optional column list: (col1, col2, ...)
        let columns = if match_token(&tokens, pos, &Token::LParen) {
            // But only consume if it's followed by an identifier (not a
            // SELECT, which would mean this is the CTE body paren).
            // Peek ahead: if the token after ( is an Ident, treat as
            // column list; otherwise it's the body.
            if pos + 1 < tokens.len() && matches!(tokens[pos + 1], Token::Ident(_)) {
                pos += 1; // consume (
                let mut cols = Vec::new();
                loop {
                    let col_name = expect_ident(&tokens, &mut pos)?;
                    cols.push(col_name);
                    if match_token(&tokens, pos, &Token::Comma) {
                        pos += 1;
                        continue;
                    }
                    break;
                }
                expect_token(&tokens, &mut pos, &Token::RParen)?;
                Some(cols)
            } else {
                None
            }
        } else {
            None
        };

        // Expect AS
        expect_keyword(&tokens, &mut pos, "AS")?;

        // Expect ( — start of CTE body
        expect_token(&tokens, &mut pos, &Token::LParen)?;

        // Capture the body tokens until the matching ).
        let body_start = pos;
        let mut depth: i32 = 1;
        while pos < tokens.len() {
            match &tokens[pos] {
                Token::LParen => depth += 1,
                Token::RParen => {
                    depth -= 1;
                    if depth == 0 {
                        break;
                    }
                }
                Token::EOF => return Err("unterminated CTE body".into()),
                _ => {}
            }
            pos += 1;
        }
        if pos >= tokens.len() {
            return Err("unterminated CTE body".into());
        }
        let body_tokens = &tokens[body_start..pos];
        expect_token(&tokens, &mut pos, &Token::RParen)?;

        // Detect a top-level UNION ALL inside the body (for recursive CTEs).
        // Walk the body tokens at depth 0 (relative to body) looking for
        // Keyword("UNION") followed by Keyword("ALL").
        let (anchor_sql, recursive_sql, body_set) = parse_cte_body(body_tokens)?;

        // Parse the body as a SetQuery (for the new `body` field).
        // If parsing fails, body_set is None but anchor/recursive strings
        // are still populated.
        ctes.push(CteDef {
            name,
            columns,
            anchor: anchor_sql,
            recursive: recursive_sql,
            body: body_set,
        });

        // Check for comma (another CTE) or end of CTE list
        if match_token(&tokens, pos, &Token::Comma) {
            pos += 1;
            continue;
        }
        break;
    }

    // The rest is the outer query, possibly with OPTION (MAXRECURSION n).
    let mut max_recursion = 100u32;
    let outer_start = pos;
    let mut outer_end = tokens.len();

    // Scan for OPTION (MAXRECURSION n) at the top level.
    let mut scan = pos;
    while scan < tokens.len() {
        match &tokens[scan] {
            Token::Keyword(k) if k == "OPTION" => {
                // Found OPTION — extract MAXRECURSION if present.
                outer_end = scan;
                scan += 1;
                // Expect (
                if !match_token(&tokens, scan, &Token::LParen) {
                    break;
                }
                scan += 1;
                while scan < tokens.len() {
                    match &tokens[scan] {
                        Token::Keyword(k) if k == "MAXRECURSION" => {
                            scan += 1;
                            if scan < tokens.len() {
                                if let Token::Int(n) = &tokens[scan] {
                                    max_recursion = *n as u32;
                                    scan += 1;
                                }
                            }
                        }
                        Token::RParen => {
                            scan += 1;
                            break;
                        }
                        _ => scan += 1,
                    }
                }
                break;
            }
            Token::EOF => {
                outer_end = scan;
                break;
            }
            _ => scan += 1,
        }
    }

    // Re-serialise the outer query tokens to a SQL string.
    let outer_query = tokens_to_sql(&tokens[outer_start..outer_end]);
    // Parse the outer query as a SetQuery.
    let mut outer_tokens = tokens[outer_start..outer_end].to_vec();
    outer_tokens.push(Token::EOF);
    let outer = parse_set(outer_tokens).ok();

    Ok(WithClause {
        ctes,
        outer_query,
        outer,
        max_recursion,
    })
}

/// Parse the CTE body tokens: detect a top-level UNION ALL and split
/// into anchor + recursive. Returns (anchor_sql, recursive_sql, body_set).
fn parse_cte_body(body_tokens: &[Token]) -> Result<(String, Option<String>, Option<SetQuery>), String> {
    // Find a top-level UNION ALL (depth 0 relative to body_tokens).
    let mut depth: i32 = 0;
    let mut union_pos: Option<usize> = None;
    let mut i = 0;
    while i < body_tokens.len() {
        match &body_tokens[i] {
            Token::LParen => depth += 1,
            Token::RParen => depth -= 1,
            Token::Keyword(k) if depth == 0 && k == "UNION" => {
                // Check if next token is ALL
                if i + 1 < body_tokens.len() {
                    if let Token::Keyword(next_k) = &body_tokens[i + 1] {
                        if next_k == "ALL" {
                            union_pos = Some(i);
                            break;
                        }
                    }
                }
            }
            _ => {}
        }
        i += 1;
    }

    let (anchor_sql, recursive_sql) = if let Some(u_pos) = union_pos {
        let anchor_tokens = &body_tokens[..u_pos];
        // Skip UNION ALL (2 tokens)
        let recursive_tokens = &body_tokens[u_pos + 2..];
        (
            tokens_to_sql(anchor_tokens),
            Some(tokens_to_sql(recursive_tokens)),
        )
    } else {
        (tokens_to_sql(body_tokens), None)
    };

    // Parse the full body as a SetQuery.
    let mut full_tokens = body_tokens.to_vec();
    full_tokens.push(Token::EOF);
    let body_set = parse_set(full_tokens).ok();

    Ok((anchor_sql, recursive_sql, body_set))
}

/// Re-serialise a token slice back to a SQL string. This is a lossy
/// conversion (whitespace is normalised to single spaces; comments are
/// lost) but is sufficient for the executor's re-parse path.
fn tokens_to_sql(tokens: &[Token]) -> String {
    let mut parts: Vec<String> = Vec::new();
    for tok in tokens {
        match tok {
            Token::Keyword(k) => parts.push(k.clone()),
            Token::Ident(s) => parts.push(s.clone()),
            Token::Int(n) => parts.push(n.to_string()),
            Token::Float(f) => parts.push(f.to_string()),
            Token::String(s) => parts.push(format!("'{}'", s.replace('\'', "''"))),
            Token::Hex(bytes) => {
                let hex: String = bytes.iter().map(|b| format!("{b:02X}")).collect();
                parts.push(format!("x'{hex}'"));
            }
            Token::Op(op) => parts.push(op.clone()),
            Token::LParen => parts.push("(".into()),
            Token::RParen => parts.push(")".into()),
            Token::Comma => parts.push(",".into()),
            Token::Semicolon => parts.push(";".into()),
            Token::Param(n) => parts.push(format!("${n}")),
            Token::QuestionMark => parts.push("?".into()),
            Token::EOF => {}
        }
    }
    parts.join(" ")
}

fn match_token(tokens: &[Token], pos: usize, expected: &Token) -> bool {
    pos < tokens.len() && tokens[pos] == *expected
}

fn match_keyword(tokens: &[Token], pos: usize, kw: &str) -> bool {
    if pos >= tokens.len() {
        return false;
    }
    matches!(&tokens[pos], Token::Keyword(k) if k == kw)
}

fn expect_keyword(tokens: &[Token], pos: &mut usize, kw: &str) -> Result<(), String> {
    if !match_keyword(tokens, *pos, kw) {
        return Err(format!(
            "expected keyword {kw}, got {:?}",
            tokens.get(*pos).unwrap_or(&Token::EOF)
        ));
    }
    *pos += 1;
    Ok(())
}

fn expect_token(tokens: &[Token], pos: &mut usize, expected: &Token) -> Result<(), String> {
    if !match_token(tokens, *pos, expected) {
        return Err(format!(
            "expected {expected:?}, got {:?}",
            tokens.get(*pos).unwrap_or(&Token::EOF)
        ));
    }
    *pos += 1;
    Ok(())
}

fn expect_ident(tokens: &[Token], pos: &mut usize) -> Result<String, String> {
    if *pos >= tokens.len() {
        return Err("expected identifier, got EOF".into());
    }
    match &tokens[*pos] {
        Token::Ident(s) => {
            *pos += 1;
            Ok(s.clone())
        }
        other => Err(format!("expected identifier, got {other:?}")),
    }
}

// -----------------------------------------------------------------------
// Tests
// -----------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_non_recursive_cte() {
        let sql = "WITH t AS (SELECT 1 AS x) SELECT * FROM t";
        let result = parse_with(sql).unwrap().unwrap();
        assert_eq!(result.ctes.len(), 1);
        assert_eq!(result.ctes[0].name, "t");
        assert!(result.ctes[0].anchor.contains("SELECT"));
        assert!(result.ctes[0].recursive.is_none());
        assert!(result.outer_query.contains("SELECT"));
        assert_eq!(result.max_recursion, 100);
    }

    #[test]
    fn parse_recursive_cte() {
        let sql = "WITH RECURSIVE countdown AS (
            SELECT 10 AS n
            UNION ALL
            SELECT n - 1 FROM countdown WHERE n > 1
        ) SELECT * FROM countdown";
        let result = parse_with(sql).unwrap().unwrap();
        assert_eq!(result.ctes.len(), 1);
        assert_eq!(result.ctes[0].name, "countdown");
        assert!(result.ctes[0].recursive.is_some());
        assert!(result.ctes[0].anchor.contains("SELECT"));
        assert!(result
            .ctes[0]
            .recursive
            .as_ref()
            .unwrap()
            .contains("countdown"));
    }

    #[test]
    fn parse_multiple_ctes() {
        let sql = "WITH a AS (SELECT 1), b AS (SELECT 2) SELECT * FROM a UNION ALL SELECT * FROM b";
        let result = parse_with(sql).unwrap().unwrap();
        assert_eq!(result.ctes.len(), 2);
        assert_eq!(result.ctes[0].name, "a");
        assert_eq!(result.ctes[1].name, "b");
    }

    #[test]
    fn parse_maxrecursion() {
        let sql = "WITH t AS (SELECT 1) SELECT * FROM t OPTION (MAXRECURSION 0)";
        let result = parse_with(sql).unwrap().unwrap();
        assert_eq!(result.max_recursion, 0);
    }

    #[test]
    fn not_a_with_clause() {
        assert!(parse_with("SELECT 1").is_none());
        assert!(parse_with("INSERT INTO t VALUES (1)").is_none());
    }

    #[test]
    fn nested_parens_in_cte() {
        let sql = "WITH t AS (SELECT * FROM (SELECT 1 AS x) sub) SELECT * FROM t";
        let result = parse_with(sql).unwrap().unwrap();
        assert_eq!(result.ctes[0].name, "t");
        assert!(result.ctes[0].anchor.contains("SELECT"));
    }

    // ===== Wave 7: Token-based parser tests =====

    #[test]
    fn parse_cte_with_column_list() {
        // WITH t(a, b) AS (SELECT x, y FROM x) SELECT a, b FROM t
        let sql = "WITH t(a, b) AS (SELECT x, y FROM x) SELECT a, b FROM t";
        let result = parse_with(sql).unwrap().unwrap();
        assert_eq!(result.ctes.len(), 1);
        assert_eq!(result.ctes[0].name, "t");
        assert_eq!(
            result.ctes[0].columns,
            Some(vec!["a".to_string(), "b".to_string()])
        );
    }

    #[test]
    fn parse_cte_string_literal_with_union_all() {
        // Wave 7 bug fix: a string literal containing "UNION ALL" must
        // NOT be mistaken for a recursive CTE separator.
        let sql = "WITH t AS (SELECT 'UNION ALL is a string' AS s) SELECT * FROM t";
        let result = parse_with(sql).unwrap().unwrap();
        assert_eq!(result.ctes.len(), 1);
        assert_eq!(result.ctes[0].name, "t");
        // The body should NOT be split into anchor + recursive.
        assert!(result.ctes[0].recursive.is_none());
    }

    #[test]
    fn parse_cte_recursive_with_string_containing_union_all() {
        // Recursive CTE where the anchor contains a string literal with
        // "UNION ALL" — the real UNION ALL must still be detected.
        let sql = "WITH RECURSIVE t AS (
            SELECT 'UNION ALL text' AS s
            UNION ALL
            SELECT s FROM t
        ) SELECT * FROM t";
        let result = parse_with(sql).unwrap().unwrap();
        assert_eq!(result.ctes[0].name, "t");
        assert!(result.ctes[0].recursive.is_some());
        // The anchor should contain the string literal.
        assert!(result.ctes[0].anchor.contains("UNION ALL text"));
    }

    #[test]
    fn parse_cte_body_parsed_as_setquery() {
        // The body field should be a parsed SetQuery.
        let sql = "WITH t AS (SELECT 1) SELECT * FROM t";
        let result = parse_with(sql).unwrap().unwrap();
        assert!(result.ctes[0].body.is_some());
        assert!(matches!(result.ctes[0].body, Some(SetQuery::Select(_))));
    }

    #[test]
    fn parse_cte_recursive_body_parsed_as_unionall() {
        let sql = "WITH RECURSIVE t AS (SELECT 1 UNION ALL SELECT 2) SELECT * FROM t";
        let result = parse_with(sql).unwrap().unwrap();
        assert!(result.ctes[0].body.is_some());
        assert!(matches!(result.ctes[0].body, Some(SetQuery::UnionAll(_, _))));
    }

    #[test]
    fn parse_cte_outer_query_parsed() {
        let sql = "WITH t AS (SELECT 1) SELECT * FROM t";
        let result = parse_with(sql).unwrap().unwrap();
        assert!(result.outer.is_some());
    }
}
