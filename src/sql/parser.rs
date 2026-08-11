//! SQL parser: recursive descent with Pratt-style expression precedence.
//!
//! The parser consumes the [`Token`] stream produced by
//! [`crate::sql::lexer::tokenize`] and produces a [`SelectQuery`]. Only the
//! standard `SELECT ... FROM ... [WHERE ...] [GROUP BY ...] [ORDER BY ...]
//! [LIMIT n]` form is supported — turboGP extensions (`APPROXIMATE`,
//! `TIER`, etc.) are handled separately by [`crate::sql::extensions`].
//!
//! ## Expression precedence (lowest → highest)
//!
//! 1. `OR`
//! 2. `AND`
//! 3. `NOT` (prefix)
//! 4. comparison (`=`, `!=`, `<`, `>`, `<=`, `>=`, `LIKE`, `IN`, `BETWEEN`)
//! 5. additive (`+`, `-`)
//! 6. multiplicative (`*`, `/`, `%`)
//! 7. unary (`-`, `+`)
//! 8. primary (literal, column, parenthesized, function call)
//!
//! ## Unified AST
//!
//! Since Wave 2 of the SQL Frontend Remediation, this module produces the
//! unified [`crate::sql::ast::Expr`] type. The historical 7-variant
//! `parser::Expr` has been deleted; `parser::Expr` and `parser::Value` are
//! now re-exports of `ast::Expr` and `ast::Value` so existing call sites
//! (`use crate::sql::parser::Expr`) continue to resolve.

use crate::sql::lexer::Token;

/// Re-export of the unified [`crate::sql::ast::Expr`] type.
///
/// Historical code that wrote `crate::sql::parser::Expr` continues to
/// resolve via this re-export. The old 7-variant `parser::Expr` enum
/// has been deleted.
pub use crate::sql::ast::{BinOp, Expr, UnaryOp, Value};

/// A parsed SELECT query.
#[derive(Debug, Clone)]
pub struct SelectQuery {
    /// The select-list items (columns, aggregates, or `*`).
    pub select: Vec<SelectItem>,
    /// The source table name.
    pub from: String,
    /// Optional JOIN clauses.
    pub joins: Vec<JoinClause>,
    /// Optional WHERE predicate.
    pub where_clause: Option<Expr>,
    /// GROUP BY column list (empty if no GROUP BY).
    pub group_by: Vec<String>,
    /// Optional HAVING predicate (Wave 60b). Filters groups after aggregation.
    /// The expression can reference aggregates (e.g. `count(*) > 2`).
    pub having: Option<Expr>,
    /// ORDER BY column list with ascending flag (`true` = ASC) and NULLS
    /// placement (Wave 8). Each entry is `(column_name, ascending, nulls_order)`.
    pub order_by: Vec<(String, bool, NullsOrder)>,
    /// Optional LIMIT row count.
    pub limit: Option<usize>,
    /// Optional OFFSET row count (Wave 8). Number of rows to skip before
    /// returning results.
    pub offset: Option<usize>,
    /// Optional FETCH FIRST n ROWS ONLY count (Wave 8). Alternative to LIMIT.
    pub fetch: Option<usize>,
    /// Whether SELECT DISTINCT was specified (Wave 60d). When true, the
    /// executor deduplicates the result rows.
    pub distinct: bool,
    /// Optional DISTINCT ON column list (Wave 8). `SELECT DISTINCT ON (a, b)`
    /// keeps only the first row of each group where (a, b) are equal.
    /// None means no DISTINCT ON clause. If Some, `distinct` is also true.
    pub distinct_on: Option<Vec<String>>,
}

/// NULLS ordering in ORDER BY clauses (Wave 8).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NullsOrder {
    /// `NULLS FIRST` — NULLs sort before non-NULLs.
    First,
    /// `NULLS LAST` — NULLs sort after non-NULLs.
    Last,
    /// No NULLS clause — use the default (NULLs last for ASC, NULLs first
    /// for DESC in most SQL dialects).
    Default,
}

/// A parsed JOIN clause.
#[derive(Debug, Clone)]
pub struct JoinClause {
    /// The table to join.
    pub table: String,
    /// The join condition (ON clause).
    pub on: Expr,
    /// Join type keyword, uppercased: `INNER`, `LEFT`, `RIGHT`, `FULL`, `CROSS`.
    /// Wave 49 fix: previously the parser recognised `LEFT JOIN` syntactically
    /// but did not propagate the join type, so the executor always used
    /// INNER. Carrying the type lets the executor dispatch the correct
    /// `JoinType` to `hash_join`.
    pub join_type: String,
}

/// One item in the SELECT list.
#[derive(Debug, Clone)]
pub enum SelectItem {
    /// A bare column reference: `col`.
    Column(String),
    /// An aggregate function call: `func(arg) [AS alias]`.
    Aggregate {
        /// The function name, uppercased (e.g. `COUNT`, `SUM`, `AVG`).
        func: String,
        /// The function argument: `*`, a column name, or a literal as a
        /// string.
        arg: String,
        /// Optional output alias.
        alias: Option<String>,
    },
    /// The `*` wildcard.
    Star,
    /// A non-negative integer literal in the SELECT list
    /// (e.g. `SELECT 1, URL, count(*) ...`). ClickBench Q15-Q42 use
    /// this to emit a constant column alongside the URL and count.
    /// Negative literals are rejected at parse time.
    Literal(u64),
    /// A window function call: `func(args) OVER (spec) [AS alias]`.
    Window {
        /// The function name, uppercased (e.g. `ROW_NUMBER`, `RANK`, `SUM`).
        func: String,
        /// The function argument (column name, or empty for ROW_NUMBER).
        arg: String,
        /// The window specification string (content of OVER (...)).
        over_spec: String,
        /// Optional output alias.
        alias: Option<String>,
    },
    /// A general expression (Wave 60a): `CASE WHEN ... THEN ... END`,
    /// arithmetic, etc. Carries the parsed Expr and an optional alias.
    /// The executor evaluates the expression per row.
    Expression {
        /// The parsed expression.
        expr: Expr,
        /// Optional output alias.
        alias: Option<String>,
    },
}

/// A set operation tree: `SELECT ... UNION SELECT ... INTERSECT SELECT ...`.
///
/// Wave 4 introduces this type to represent SQL set operations. The
/// leaves are [`SelectQuery`] instances; internal nodes are set
/// operators (`UNION`, `UNION ALL`, `INTERSECT`, `EXCEPT`).
///
/// # Precedence
///
/// `INTERSECT` binds tighter than `UNION` and `EXCEPT`, matching the
/// SQL standard. `UNION` and `EXCEPT` are left-associative and have
/// equal precedence.
#[derive(Debug, Clone)]
pub enum SetQuery {
    /// A single SELECT query (no set operation).
    Select(SelectQuery),
    /// `left UNION right` — deduplicates rows across both inputs.
    Union(Box<SetQuery>, Box<SetQuery>),
    /// `left UNION ALL right` — preserves duplicates.
    UnionAll(Box<SetQuery>, Box<SetQuery>),
    /// `left INTERSECT right` — keeps rows present in both inputs.
    Intersect(Box<SetQuery>, Box<SetQuery>),
    /// `left EXCEPT right` — keeps rows in left but not in right.
    Except(Box<SetQuery>, Box<SetQuery>),
}

impl SetQuery {
    /// If this `SetQuery` is a single SELECT (no set operation), return
    /// a reference to the inner [`SelectQuery`]. Returns `None` for
    /// `Union`, `UnionAll`, `Intersect`, `Except`.
    pub fn as_select(&self) -> Option<&SelectQuery> {
        match self {
            SetQuery::Select(q) => Some(q),
            _ => None,
        }
    }

    /// If this `SetQuery` is a single SELECT, return the inner
    /// [`SelectQuery`] by value. Returns `Err` with a human-readable
    /// message for set operations (which cannot be flattened to a
    /// single SELECT).
    pub fn into_select(self) -> Result<SelectQuery, String> {
        match self {
            SetQuery::Select(q) => Ok(q),
            other => Err(format!("expected single SELECT, got set operation: {other:?}")),
        }
    }
}

impl std::fmt::Display for SetQuery {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            SetQuery::Select(q) => write!(f, "{q:?}"),
            SetQuery::Union(l, r) => write!(f, "({l} UNION {r})"),
            SetQuery::UnionAll(l, r) => write!(f, "({l} UNION ALL {r})"),
            SetQuery::Intersect(l, r) => write!(f, "({l} INTERSECT {r})"),
            SetQuery::Except(l, r) => write!(f, "({l} EXCEPT {r})"),
        }
    }
}

/// Parse a token stream into a [`SelectQuery`].
///
/// **Wave 4:** This function now delegates to [`parse_set`] and extracts
/// the single `SelectQuery` for backward compatibility. For queries that
/// contain set operations (`UNION`, `INTERSECT`, `EXCEPT`), use
/// [`parse_set`] directly.
///
/// # Errors
///
/// Returns `Err(String)` with a human-readable message for any malformed
/// input — missing keywords, unexpected tokens, unterminated expressions,
/// etc. Also returns `Err` if the parsed query is a set operation (use
/// [`parse_set`] to handle those). The error message is intended for
/// display to a human, not for programmatic matching.
pub fn parse(tokens: Vec<Token>) -> Result<SelectQuery, String> {
    let set = parse_set(tokens)?;
    set.into_select()
}

/// Parse a token stream into a [`SetQuery`], which may be a single
/// `SELECT` or a tree of set operations (`UNION`, `UNION ALL`,
/// `INTERSECT`, `EXCEPT`).
///
/// Set operation precedence (per SQL standard):
/// 1. `INTERSECT` binds tightest
/// 2. `UNION` and `EXCEPT` are left-associative with equal precedence
///
/// Parenthesised set operations are supported: `(SELECT ... UNION SELECT ...)
/// ORDER BY ...` parses with the parenthesised body as one operand.
///
/// # Errors
///
/// Returns `Err(String)` for malformed input.
pub fn parse_set(tokens: Vec<Token>) -> Result<SetQuery, String> {
    let mut p = Parser::new(tokens);
    let q = p.parse_set_expr()?;
    match p.peek() {
        Token::Semicolon | Token::EOF => Ok(q),
        other => Err(format!("unexpected trailing token: {other:?}")),
    }
}

/// Parse a single SQL expression from a token slice. Used by the DML
/// parser to parse SET expressions and WHERE predicates without
/// wrapping them in a synthetic SELECT.
///
/// The token slice should NOT include a trailing EOF — this function
/// adds one internally. All tokens in the slice are consumed (an error
/// is returned if trailing tokens remain after the expression).
///
/// # Errors
///
/// Returns `Err(String)` for malformed expressions or trailing tokens.
pub fn parse_expression(tokens: Vec<Token>) -> Result<Expr, String> {
    let mut p = Parser::new(tokens);
    let e = p.parse_expr()?;
    match p.peek() {
        Token::Semicolon | Token::EOF => Ok(e),
        other => Err(format!("unexpected trailing token in expression: {other:?}")),
    }
}

/// The internal recursive-descent parser state.
struct Parser {
    /// The token stream (with trailing EOF).
    tokens: Vec<Token>,
    /// Current cursor position.
    pos: usize,
}

impl Parser {
    fn new(tokens: Vec<Token>) -> Self {
        Self { tokens, pos: 0 }
    }

    /// Peek at the current token (without advancing).
    fn peek(&self) -> &Token {
        &self.tokens[self.pos]
    }

    /// Consume and return the current token. Does not advance past EOF.
    fn next(&mut self) -> Token {
        let t = self.tokens.get(self.pos).cloned().unwrap_or(Token::EOF);
        if !matches!(t, Token::EOF) {
            self.pos += 1;
        }
        t
    }

    /// If the current token is `Keyword(kw)` (case-sensitive, since keywords
    /// are uppercased by the lexer), consume it and return `true`.
    fn match_keyword(&mut self, kw: &str) -> bool {
        if let Token::Keyword(k) = self.peek() {
            if k == kw {
                self.pos += 1;
                return true;
            }
        }
        false
    }

    /// Consume the current token if it is `Keyword(kw)`, else error.
    fn expect_keyword(&mut self, kw: &str) -> Result<(), String> {
        if self.match_keyword(kw) {
            return Ok(());
        }
        Err(format!("expected keyword {kw}, got {:?}", self.peek()))
    }

    /// If the current token is `Op(op)`, consume it and return `true`.
    fn match_op(&mut self, op: &str) -> bool {
        if let Token::Op(o) = self.peek() {
            if o == op {
                self.pos += 1;
                return true;
            }
        }
        false
    }

    /// If the current token is an `Ident` matching `name` (case-insensitive),
    /// consume it and return `true`. Used for non-keyword reserved words
    /// (`LIMIT`, `ASC`, `DESC`).
    fn match_ident(&mut self, name: &str) -> bool {
        match self.peek() {
            Token::Ident(s) if s.eq_ignore_ascii_case(name) => {
                self.pos += 1;
                true
            }
            Token::Keyword(k) if k.eq_ignore_ascii_case(name) => {
                self.pos += 1;
                true
            }
            _ => false,
        }
    }

    // -----------------------------------------------------------------------
    // Set expression (UNION / INTERSECT / EXCEPT)
    // -----------------------------------------------------------------------

    /// Parse a set expression: a chain of SELECTs joined by set operators.
    /// Precedence: INTERSECT > UNION = EXCEPT (left-associative).
    fn parse_set_expr(&mut self) -> Result<SetQuery, String> {
        // Left-associative chain of UNION / EXCEPT, each operand being an
        // INTERSECT chain.
        let mut left = self.parse_intersect_expr()?;
        loop {
            if self.match_keyword("UNION") {
                let all = self.match_keyword("ALL");
                let right = self.parse_intersect_expr()?;
                left = if all {
                    SetQuery::UnionAll(Box::new(left), Box::new(right))
                } else {
                    SetQuery::Union(Box::new(left), Box::new(right))
                };
            } else if self.match_keyword("EXCEPT") {
                let _ = self.match_keyword("ALL"); // EXCEPT ALL is non-standard but tolerated
                let right = self.parse_intersect_expr()?;
                left = SetQuery::Except(Box::new(left), Box::new(right));
            } else {
                break;
            }
        }
        Ok(left)
    }

    /// Parse an INTERSECT chain (binds tighter than UNION/EXCEPT).
    fn parse_intersect_expr(&mut self) -> Result<SetQuery, String> {
        let mut left = self.parse_set_primary()?;
        while self.match_keyword("INTERSECT") {
            let _ = self.match_keyword("ALL");
            let right = self.parse_set_primary()?;
            left = SetQuery::Intersect(Box::new(left), Box::new(right));
        }
        Ok(left)
    }

    /// Parse a single set operand: either a parenthesised set expression
    /// or a single SELECT.
    fn parse_set_primary(&mut self) -> Result<SetQuery, String> {
        // Parenthesised set expression: ( SELECT ... UNION SELECT ... )
        if matches!(self.peek(), Token::LParen) {
            // Save position so we can backtrack if the parens turn out to
            // be part of a primary expression rather than a set operand.
            let save = self.pos;
            self.next(); // consume (
            // Peek: if the next token is SELECT, this is a parenthesised
            // set expression. Otherwise, it's a parenthesised expression
            // and we should let parse_select handle it.
            if matches!(self.peek(), Token::Keyword(k) if k == "SELECT") {
                let inner = self.parse_set_expr()?;
                if !matches!(self.peek(), Token::RParen) {
                    return Err(format!("expected ) after set expression, got {:?}", self.peek()));
                }
                self.next(); // consume )
                // TODO: attach ORDER BY / LIMIT to the parenthesised body.
                return Ok(inner);
            }
            // Not a parenthesised set expression; restore and fall through.
            self.pos = save;
        }
        let q = self.parse_select()?;
        Ok(SetQuery::Select(q))
    }

    // -----------------------------------------------------------------------
    // SELECT statement
    // -----------------------------------------------------------------------

    fn parse_select(&mut self) -> Result<SelectQuery, String> {
        self.expect_keyword("SELECT")?;
        // Wave 60d: SELECT DISTINCT — consume the DISTINCT keyword and set
        // the distinct flag. The executor deduplicates the result rows.
        // Wave 8: DISTINCT ON (cols) — consume DISTINCT, then optional ON (cols).
        let mut distinct_on: Option<Vec<String>> = None;
        let distinct = if self.match_keyword("DISTINCT") {
            // Check for ON (col1, col2, ...)
            if self.match_keyword("ON") {
                if !matches!(self.peek(), Token::LParen) {
                    return Err("expected ( after DISTINCT ON".into());
                }
                self.next(); // consume (
                let mut cols = Vec::new();
                loop {
                    let col = match self.peek().clone() {
                        Token::Ident(s) => {
                            self.next();
                            s
                        }
                        other => return Err(format!("expected column in DISTINCT ON, got {other:?}")),
                    };
                    cols.push(col);
                    match self.peek() {
                        Token::Comma => {
                            self.next();
                        }
                        Token::RParen => {
                            self.next();
                            break;
                        }
                        other => return Err(format!("expected , or ) in DISTINCT ON, got {other:?}")),
                    }
                }
                distinct_on = Some(cols);
            }
            true
        } else {
            false
        };
        let select = self.parse_select_list()?;
        // FROM is optional — allows `SELECT 1` and `SELECT count(*)`.
        let from = if self.match_keyword("FROM") {
            self.parse_table_name()?
        } else {
            // No FROM clause — use a synthetic single-row table.
            // This allows `SELECT 1`, `SELECT count(*)`, etc.
            "__dummy__".to_string()
        };

        // Parse optional JOIN clauses
        let joins = self.parse_joins()?;

        let where_clause =
            if self.match_keyword("WHERE") { Some(self.parse_expr()?) } else { None };

        let group_by = if self.match_keyword("GROUP") {
            self.expect_keyword("BY")?;
            self.parse_column_list()?
        } else {
            Vec::new()
        };

        // Wave 60b: HAVING clause — parsed after GROUP BY.
        let having = if self.match_keyword("HAVING") { Some(self.parse_expr()?) } else { None };

        let order_by = if self.match_keyword("ORDER") {
            self.expect_keyword("BY")?;
            self.parse_order_list()?
        } else {
            Vec::new()
        };

        // Wave 8: LIMIT / OFFSET / FETCH FIRST n ROWS ONLY
        let limit = if self.match_ident("LIMIT") { Some(self.parse_usize()?) } else { None };
        let offset = if self.match_keyword("OFFSET") {
            let n = self.parse_usize()?;
            // Optional ROWS keyword
            let _ = self.match_keyword("ROWS");
            Some(n)
        } else {
            None
        };
        let fetch = if self.match_keyword("FETCH") {
            // Optional FIRST or NEXT keyword
            let _ = self.match_keyword("FIRST");
            let _ = self.match_keyword("NEXT");
            let n = self.parse_usize()?;
            // Optional ROWS keyword
            let _ = self.match_keyword("ROWS");
            // Optional ONLY keyword
            let _ = self.match_keyword("ONLY");
            Some(n)
        } else {
            None
        };

        Ok(SelectQuery {
            select,
            from,
            joins,
            where_clause,
            group_by,
            having,
            order_by,
            limit,
            offset,
            fetch,
            distinct,
            distinct_on,
        })
    }

    /// Parse zero or more JOIN clauses.
    ///
    /// Wave 49 fix: every branch now records the join type (`INNER`, `LEFT`,
    /// `RIGHT`, `FULL`, `CROSS`) on the `JoinClause`. Previously all joins
    /// silently became INNER at execution time because the executor never
    /// saw the original keyword.
    fn parse_joins(&mut self) -> Result<Vec<JoinClause>, String> {
        let mut joins = Vec::new();
        loop {
            // Bare `JOIN` is INNER.
            if self.match_keyword("JOIN") {
                let table = self.parse_table_name()?;
                self.expect_keyword("ON")?;
                let on = self.parse_expr()?;
                joins.push(JoinClause { table, on, join_type: "INNER".to_string() });
            } else if self.match_keyword("INNER") {
                self.expect_keyword("JOIN")?;
                let table = self.parse_table_name()?;
                self.expect_keyword("ON")?;
                let on = self.parse_expr()?;
                joins.push(JoinClause { table, on, join_type: "INNER".to_string() });
            } else if self.match_keyword("LEFT") {
                let _ = self.match_keyword("OUTER");
                self.expect_keyword("JOIN")?;
                let table = self.parse_table_name()?;
                self.expect_keyword("ON")?;
                let on = self.parse_expr()?;
                joins.push(JoinClause { table, on, join_type: "LEFT".to_string() });
            } else if self.match_keyword("RIGHT") {
                let _ = self.match_keyword("OUTER");
                self.expect_keyword("JOIN")?;
                let table = self.parse_table_name()?;
                self.expect_keyword("ON")?;
                let on = self.parse_expr()?;
                joins.push(JoinClause { table, on, join_type: "RIGHT".to_string() });
            } else if self.match_keyword("FULL") {
                let _ = self.match_keyword("OUTER");
                self.expect_keyword("JOIN")?;
                let table = self.parse_table_name()?;
                self.expect_keyword("ON")?;
                let on = self.parse_expr()?;
                joins.push(JoinClause { table, on, join_type: "FULL".to_string() });
            } else if self.match_keyword("CROSS") {
                self.expect_keyword("JOIN")?;
                let table = self.parse_table_name()?;
                // CROSS JOIN has no ON clause — synthesize a trivially-true
                // predicate (literal 1) so the downstream executor's
                // `extract_join_keys` / `hash_join` path does not require one.
                let on = Expr::Literal(Value::Int(1));
                joins.push(JoinClause { table, on, join_type: "CROSS".to_string() });
            } else {
                break;
            }
        }
        Ok(joins)
    }

    fn parse_select_list(&mut self) -> Result<Vec<SelectItem>, String> {
        let mut items = Vec::new();
        loop {
            items.push(self.parse_select_item()?);
            if !matches!(self.peek(), Token::Comma) {
                break;
            }
            self.next(); // consume comma
        }
        Ok(items)
    }

    fn parse_select_item(&mut self) -> Result<SelectItem, String> {
        // `*` → Star.
        if self.match_op("*") {
            return Ok(SelectItem::Star);
        }
        // Wave 60a: CASE WHEN expression in the SELECT list.
        // Wave 67: EXTRACT / CAST also produce SelectItem::Expression.
        if let Token::Keyword(kw) = self.peek() {
            if kw == "CASE" || kw == "EXTRACT" || kw == "CAST" {
                let expr = self.parse_expr()?;
                let alias = self.parse_optional_alias()?;
                return Ok(SelectItem::Expression { expr, alias });
            }
        }
        // Wave 4: Scalar subquery or parenthesised expression in SELECT list.
        // `(SELECT ...)` produces a scalar subquery; `(a + b)` produces an
        // arithmetic expression. Both are wrapped in SelectItem::Expression.
        if matches!(self.peek(), Token::LParen) {
            let expr = self.parse_expr()?;
            let alias = self.parse_optional_alias()?;
            return Ok(SelectItem::Expression { expr, alias });
        }
        // Wave 4: Unary minus / plus in SELECT list (e.g. SELECT -1 FROM t).
        if matches!(self.peek(), Token::Op(op) if op == "-" || op == "+") {
            let expr = self.parse_expr()?;
            let alias = self.parse_optional_alias()?;
            return Ok(SelectItem::Expression { expr, alias });
        }
        // Wave 5: String / Float / Hex literals in SELECT list (used by
        // DML expression parser when it wraps expressions in synthetic
        // SELECT <expr> FROM __dummy__).
        if matches!(self.peek(), Token::String(_) | Token::Float(_) | Token::Hex(_)) {
            let expr = self.parse_expr()?;
            let alias = self.parse_optional_alias()?;
            return Ok(SelectItem::Expression { expr, alias });
        }
        // Non-negative integer literal → `Literal(u64)`
        // (e.g. `SELECT 1, URL, count(*) ...` in ClickBench Q15-Q42).
        if let Token::Int(i) = self.peek().clone() {
            if i < 0 {
                return Err(format!("negative integer literal in SELECT list: {i}"));
            }
            self.next();
            // An alias (`SELECT 1 AS x`) is legal but unused — consume it.
            let _ = self.parse_optional_alias()?;
            return Ok(SelectItem::Literal(i as u64));
        }
        // `IDENT ( ... )` → Aggregate; `IDENT` → Column.
        if let Token::Ident(name) = self.peek().clone() {
            self.next();
            if matches!(self.peek(), Token::LParen) {
                self.next(); // consume (

                // Detect `func(DISTINCT col)` and normalise to
                // `Aggregate { func: "FUNC_DISTINCT", arg: "col" }`. The
                // `DISTINCT` token is not in `KEYWORDS` (it is rarely used
                // outside aggregates), so it arrives here as `Ident("DISTINCT")`.
                // `match_ident` does a case-insensitive compare, matching
                // `distinct`, `Distinct`, etc.
                let (func, arg) = if self.match_ident("DISTINCT") {
                    let col = match self.peek().clone() {
                        Token::Ident(s) => {
                            self.next();
                            s
                        }
                        other => {
                            return Err(format!(
                                "expected column name after DISTINCT, got {other:?}"
                            ));
                        }
                    };
                    (format!("{}_DISTINCT", name.to_uppercase()), col)
                } else {
                    let arg = self.parse_agg_arg()?;
                    (name.to_uppercase(), arg)
                };

                if !matches!(self.peek(), Token::RParen) {
                    return Err(format!("expected ) after aggregate arg, got {:?}", self.peek()));
                }
                self.next(); // consume )

                // Check for OVER (...) — window function.
                if self.match_keyword("OVER") {
                    if !matches!(self.peek(), Token::LParen) {
                        return Err("expected ( after OVER".into());
                    }
                    self.next(); // consume (
                                 // Collect everything until matching ).
                    let mut depth = 1i32;
                    let mut spec_parts: Vec<String> = Vec::new();
                    while depth > 0 {
                        match self.peek().clone() {
                            Token::LParen => {
                                depth += 1;
                                spec_parts.push("(".into());
                                self.next();
                            }
                            Token::RParen => {
                                depth -= 1;
                                if depth > 0 {
                                    spec_parts.push(")".into());
                                }
                                self.next();
                            }
                            Token::EOF => return Err("unterminated OVER (...)".into()),
                            other => {
                                let s = format!("{other:?}");
                                let trimmed = s.trim_matches('"').trim_matches('\'');
                                spec_parts.push(trimmed.to_string());
                                self.next();
                            }
                        }
                    }
                    let over_spec = spec_parts.join(" ");
                    let alias = self.parse_optional_alias()?;
                    return Ok(SelectItem::Window { func, arg, over_spec, alias });
                }

                let alias = self.parse_optional_alias()?;
                return Ok(SelectItem::Aggregate { func, arg, alias });
            }
            let _ = self.parse_optional_alias()?;
            // Qualified column reference: t.col or t.*
            let name = if let Token::Op(op) = self.peek() {
                if op == "." {
                    self.next(); // consume .
                    match self.peek().clone() {
                        Token::Ident(s) => {
                            self.next();
                            format!("{name}.{s}")
                        }
                        Token::Keyword(k) => {
                            self.next();
                            format!("{name}.{k}")
                        }
                        Token::Op(o) if o == "*" => {
                            self.next();
                            format!("{name}.*")
                        }
                        other => return Err(format!("expected name after '.', got {other:?}")),
                    }
                } else {
                    name
                }
            } else {
                name
            };
            return Ok(SelectItem::Column(name));
        }
        Err(format!("expected select item, got {:?}", self.peek()))
    }

    /// Parse the argument of an aggregate function: `*`, a column name,
    /// an integer literal, or an arithmetic expression like `col1 * (1 - col2)`.
    ///
    /// The expression is captured as a raw string for the executor to evaluate.
    /// Supported operators: + - * / and parentheses.
    fn parse_agg_arg(&mut self) -> Result<String, String> {
        if self.match_op("*") {
            return Ok("*".to_string());
        }
        // Parse an arithmetic expression by collecting tokens until ).
        let mut parts: Vec<String> = Vec::new();
        let mut paren_depth = 0i32;
        loop {
            match self.peek().clone() {
                Token::RParen if paren_depth == 0 => break,
                Token::RParen => {
                    paren_depth -= 1;
                    parts.push(")".into());
                    self.next();
                }
                Token::LParen => {
                    paren_depth += 1;
                    parts.push("(".into());
                    self.next();
                }
                Token::Comma | Token::Semicolon | Token::EOF => break,
                Token::Keyword(k)
                    if k == "FROM"
                        || k == "WHERE"
                        || k == "GROUP"
                        || k == "ORDER"
                        || k == "HAVING"
                        || k == "LIMIT" =>
                {
                    break
                }
                Token::Ident(name) => {
                    parts.push(name);
                    self.next();
                }
                Token::Int(i) => {
                    parts.push(i.to_string());
                    self.next();
                }
                Token::Float(f) => {
                    parts.push(f.to_string());
                    self.next();
                }
                Token::Op(op) => {
                    parts.push(op);
                    self.next();
                }
                Token::Keyword(k) => {
                    parts.push(k);
                    self.next();
                }
                _ => break,
            }
        }
        // Wave 53: allow empty args for no-argument window functions like
        // ROW_NUMBER() and DENSE_RANK(). Previously this returned an error,
        // which meant `SELECT ROW_NUMBER() OVER (...) FROM t` couldn't parse.
        if parts.is_empty() {
            return Ok(String::new());
        }
        Ok(parts.join(" "))
    }

    /// Parse an optional `AS ident` alias. Implicit aliases (without `AS`)
    /// are not supported to avoid swallowing `LIMIT` / `ASC` / `DESC`.
    fn parse_optional_alias(&mut self) -> Result<Option<String>, String> {
        if self.match_keyword("AS") {
            if let Token::Ident(name) = self.peek().clone() {
                self.next();
                return Ok(Some(name));
            }
            return Err(format!("expected alias after AS, got {:?}", self.peek()));
        }
        Ok(None)
    }

    fn parse_table_name(&mut self) -> Result<String, String> {
        // Reserved keywords that cannot be used as table names.
        const RESERVED: &[&str] = &[
            "WHERE",
            "GROUP",
            "ORDER",
            "HAVING",
            "LIMIT",
            "JOIN",
            "ON",
            "LEFT",
            "RIGHT",
            "INNER",
            "OUTER",
            "UNION",
            "EXCEPT",
            "INTERSECT",
        ];
        let first = match self.peek().clone() {
            Token::Ident(name) => name,
            Token::Keyword(k) => {
                if RESERVED.contains(&k.as_str()) {
                    return Err(format!("expected table name, got keyword '{k}'"));
                }
                k
            }
            other => return Err(format!("expected table name, got {other:?}")),
        };
        self.next();
        // Check for schema.table
        if let Token::Op(op) = self.peek() {
            if op == "." {
                self.next(); // consume .
                let second = match self.peek().clone() {
                    Token::Ident(name) => name,
                    Token::Keyword(k) => k,
                    other => return Err(format!("expected name after '.', got {other:?}")),
                };
                self.next();
                return Ok(format!("{first}.{second}"));
            }
        }
        Ok(first)
    }

    fn parse_column_list(&mut self) -> Result<Vec<String>, String> {
        let mut cols = Vec::new();
        loop {
            match self.peek().clone() {
                Token::Ident(name) => {
                    self.next();
                    cols.push(name);
                }
                // Positional GROUP BY reference (e.g. `GROUP BY 1, URL`).
                // The integer refers to the Nth SELECT item; when that item
                // is a literal constant (as in ClickBench Q15-Q42 where
                // `1` refers to `SELECT 1`), every row shares the same value
                // so it has no effect on grouping. We simply skip it —
                // only the non-numeric GROUP BY items remain, which is
                // semantically correct for the ClickBench queries.
                Token::Int(_) => {
                    self.next();
                }
                other => {
                    return Err(format!("expected column name in GROUP BY, got {other:?}"));
                }
            }
            if !matches!(self.peek(), Token::Comma) {
                break;
            }
            self.next();
        }
        Ok(cols)
    }

    fn parse_order_list(&mut self) -> Result<Vec<(String, bool, NullsOrder)>, String> {
        let mut items = Vec::new();
        loop {
            if let Token::Ident(name) = self.peek().clone() {
                self.next();
                let ascending = if self.match_ident("DESC") {
                    false
                } else {
                    let _ = self.match_ident("ASC");
                    true
                };
                // Wave 8: NULLS FIRST / NULLS LAST
                let nulls_order = if self.match_keyword("NULLS") {
                    if self.match_keyword("FIRST") {
                        NullsOrder::First
                    } else if self.match_keyword("LAST") {
                        NullsOrder::Last
                    } else {
                        return Err("expected FIRST or LAST after NULLS".into());
                    }
                } else {
                    NullsOrder::Default
                };
                items.push((name, ascending, nulls_order));
            } else {
                return Err(format!("expected column name in ORDER BY, got {:?}", self.peek()));
            }
            if !matches!(self.peek(), Token::Comma) {
                break;
            }
            self.next();
        }
        Ok(items)
    }

    fn parse_usize(&mut self) -> Result<usize, String> {
        if let Token::Int(i) = self.peek() {
            if *i < 0 {
                return Err(format!("expected non-negative integer, got {i}"));
            }
            let u = *i as usize;
            self.next();
            return Ok(u);
        }
        Err(format!("expected integer, got {:?}", self.peek()))
    }

    // -----------------------------------------------------------------------
    // Expression parsing (Pratt-style by precedence climbing)
    // -----------------------------------------------------------------------

    fn parse_expr(&mut self) -> Result<Expr, String> {
        self.parse_or_expr()
    }

    fn parse_or_expr(&mut self) -> Result<Expr, String> {
        let mut left = self.parse_and_expr()?;
        while self.match_keyword("OR") {
            let right = self.parse_and_expr()?;
            left = Expr::binary(left, BinOp::Or, right);
        }
        Ok(left)
    }

    fn parse_and_expr(&mut self) -> Result<Expr, String> {
        let mut left = self.parse_not_expr()?;
        while self.match_keyword("AND") {
            let right = self.parse_not_expr()?;
            left = Expr::binary(left, BinOp::And, right);
        }
        Ok(left)
    }

    /// Parse a prefix `NOT` expression. NOT has lower precedence than
    /// comparison but higher than AND, so `NOT a = 1 AND b = 2` parses as
    /// `(NOT (a = 1)) AND (b = 2)`.
    ///
    /// Also handles `EXISTS (SELECT ...)` and `NOT EXISTS (SELECT ...)`
    /// as prefix predicates (EXISTS is technically a primary expression
    /// but is most naturally handled alongside NOT since both are prefix
    /// logical operators).
    fn parse_not_expr(&mut self) -> Result<Expr, String> {
        if self.match_keyword("NOT") {
            // NOT EXISTS (SELECT ...)
            if self.match_keyword("EXISTS") {
                if self.peek() != &Token::LParen {
                    return Err("expected ( after EXISTS".into());
                }
                self.next(); // consume (
                if !matches!(self.peek(), Token::Keyword(k) if k == "SELECT") {
                    return Err("expected SELECT after EXISTS (".into());
                }
                let subquery_sql = self.capture_subquery_sql()?;
                if !matches!(self.peek(), Token::RParen) {
                    return Err(format!("expected ) after EXISTS subquery, got {:?}", self.peek()));
                }
                self.next(); // consume )
                return Ok(Expr::Exists { subquery_sql, negated: true });
            }
            let inner = self.parse_not_expr()?;
            return Ok(Expr::Not(Box::new(inner)));
        }
        // EXISTS (SELECT ...) — non-negated form
        if self.match_keyword("EXISTS") {
            if self.peek() != &Token::LParen {
                return Err("expected ( after EXISTS".into());
            }
            self.next(); // consume (
            if !matches!(self.peek(), Token::Keyword(k) if k == "SELECT") {
                return Err("expected SELECT after EXISTS (".into());
            }
            let subquery_sql = self.capture_subquery_sql()?;
            if !matches!(self.peek(), Token::RParen) {
                return Err(format!("expected ) after EXISTS subquery, got {:?}", self.peek()));
            }
            self.next(); // consume )
            return Ok(Expr::Exists { subquery_sql, negated: false });
        }
        self.parse_comparison_expr()
    }

    fn parse_comparison_expr(&mut self) -> Result<Expr, String> {
        let left = self.parse_additive_expr()?;
        // IS NULL / IS NOT NULL
        if self.match_keyword("IS") {
            let negated = self.match_keyword("NOT");
            if !self.match_keyword("NULL") {
                return Err("expected NULL after IS [NOT]".into());
            }
            return Ok(Expr::IsNull { expr: Box::new(left), negated });
        }
        // Comparison operators: = != <> < > <= >=
        if let Token::Op(op) = self.peek().clone() {
            if let Some(binop) = BinOp::from_str(&op) {
                if binop.is_comparison() {
                    self.next();
                    let right = self.parse_additive_expr()?;
                    return Ok(Expr::binary(left, binop, right));
                }
            }
        }
        // LIKE / NOT LIKE
        if self.match_ident("LIKE") {
            let right = self.parse_additive_expr()?;
            return Ok(Expr::Like {
                expr: Box::new(left),
                pattern: Box::new(right),
                negated: false,
            });
        }
        if self.match_keyword("NOT") {
            if self.match_ident("LIKE") {
                let right = self.parse_additive_expr()?;
                return Ok(Expr::Like {
                    expr: Box::new(left),
                    pattern: Box::new(right),
                    negated: true,
                });
            }
            // NOT could be part of another construct; put it back
            self.pos -= 1;
        }
        // BETWEEN x AND y
        if self.match_keyword("BETWEEN") {
            let low = self.parse_additive_expr()?;
            if !self.match_keyword("AND") {
                return Err("expected AND after BETWEEN low".into());
            }
            let high = self.parse_additive_expr()?;
            return Ok(Expr::Between {
                expr: Box::new(left),
                low: Box::new(low),
                high: Box::new(high),
                negated: false,
            });
        }
        // NOT BETWEEN
        if self.match_keyword("NOT") {
            if self.match_keyword("BETWEEN") {
                let low = self.parse_additive_expr()?;
                if !self.match_keyword("AND") {
                    return Err("expected AND after NOT BETWEEN low".into());
                }
                let high = self.parse_additive_expr()?;
                return Ok(Expr::Between {
                    expr: Box::new(left),
                    low: Box::new(low),
                    high: Box::new(high),
                    negated: true,
                });
            }
            self.pos -= 1;
        }
        // IN (val1, val2, ...) or IN (SELECT ...)
        if self.match_keyword("IN") {
            if self.peek() != &Token::LParen {
                return Err("expected ( after IN".into());
            }
            self.next(); // consume (
            // Check if this is a subquery (SELECT ...) or a value list.
            if matches!(self.peek(), Token::Keyword(k) if k == "SELECT") {
                // IN (SELECT ...) — parse a subquery and capture its SQL.
                let subquery_sql = self.capture_subquery_sql()?;
                if !matches!(self.peek(), Token::RParen) {
                    return Err(format!(
                        "expected ) after IN subquery, got {:?}",
                        self.peek()
                    ));
                }
                self.next(); // consume )
                return Ok(Expr::InSubquery {
                    expr: Box::new(left),
                    subquery_sql,
                    negated: false,
                });
            }
            let mut list: Vec<Expr> = Vec::new();
            loop {
                list.push(self.parse_additive_expr()?);
                match self.peek() {
                    Token::Comma => {
                        self.next();
                    }
                    Token::RParen => {
                        self.next();
                        break;
                    }
                    other => return Err(format!("expected , or ) in IN list, got {other:?}")),
                }
            }
            return Ok(Expr::InList { expr: Box::new(left), list, negated: false });
        }
        // NOT IN (val1, ...) or NOT IN (SELECT ...)
        if self.match_keyword("NOT") {
            if self.match_keyword("IN") {
                if self.peek() != &Token::LParen {
                    return Err("expected ( after NOT IN".into());
                }
                self.next();
                if matches!(self.peek(), Token::Keyword(k) if k == "SELECT") {
                    let subquery_sql = self.capture_subquery_sql()?;
                    if !matches!(self.peek(), Token::RParen) {
                        return Err(format!(
                            "expected ) after NOT IN subquery, got {:?}",
                            self.peek()
                        ));
                    }
                    self.next(); // consume )
                    return Ok(Expr::InSubquery {
                        expr: Box::new(left),
                        subquery_sql,
                        negated: true,
                    });
                }
                let mut list: Vec<Expr> = Vec::new();
                loop {
                    list.push(self.parse_additive_expr()?);
                    match self.peek() {
                        Token::Comma => {
                            self.next();
                        }
                        Token::RParen => {
                            self.next();
                            break;
                        }
                        other => {
                            return Err(format!("expected , or ) in NOT IN list, got {other:?}"))
                        }
                    }
                }
                return Ok(Expr::InList { expr: Box::new(left), list, negated: true });
            }
            self.pos -= 1;
        }
        Ok(left)
    }

    /// Capture the raw SQL text of a subquery (from the current token
    /// position to the matching closing parenthesis). Used for
    /// `IN (SELECT ...)` and `EXISTS (SELECT ...)` where the subquery
    /// is re-parsed by the executor at runtime.
    ///
    /// The opening `(` is assumed to already be consumed; this function
    /// consumes through the matching `)` (but the caller is responsible
    /// for consuming the final `)` — actually no, this function consumes
    /// the closing `)` too. Wait, let me re-check: the caller checks for
    /// `)` after this returns, so this function should NOT consume it.
    /// Actually, looking at the callers above, they DO consume the `)`.
    /// So this function captures tokens until the matching `)` (without
    /// consuming it) and joins them into a SQL string.
    fn capture_subquery_sql(&mut self) -> Result<String, String> {
        let mut depth: i32 = 0;
        let mut parts: Vec<String> = Vec::new();
        loop {
            match self.peek().clone() {
                Token::EOF => return Err("unterminated subquery".into()),
                Token::LParen => {
                    depth += 1;
                    parts.push("(".into());
                    self.next();
                }
                Token::RParen => {
                    if depth == 0 {
                        break;
                    }
                    depth -= 1;
                    parts.push(")".into());
                    self.next();
                }
                Token::Keyword(k) => {
                    parts.push(k);
                    self.next();
                }
                Token::Ident(s) => {
                    parts.push(s);
                    self.next();
                }
                Token::Int(i) => {
                    parts.push(i.to_string());
                    self.next();
                }
                Token::Float(f) => {
                    parts.push(f.to_string());
                    self.next();
                }
                Token::String(s) => {
                    parts.push(format!("'{}'", s.replace('\'', "''")));
                    self.next();
                }
                Token::Hex(bytes) => {
                    let hex: String = bytes.iter().map(|b| format!("{b:02X}")).collect();
                    parts.push(format!("x'{hex}'"));
                    self.next();
                }
                Token::Op(op) => {
                    parts.push(op);
                    self.next();
                }
                Token::Comma => {
                    parts.push(",".into());
                    self.next();
                }
                Token::Semicolon => {
                    parts.push(";".into());
                    self.next();
                }
                Token::Param(n) => {
                    parts.push(format!("${n}"));
                    self.next();
                }
                Token::QuestionMark => {
                    parts.push("?".into());
                    self.next();
                }
            }
        }
        Ok(parts.join(" "))
    }

    fn parse_additive_expr(&mut self) -> Result<Expr, String> {
        let mut left = self.parse_multiplicative_expr()?;
        loop {
            if let Token::Op(op) = self.peek().clone() {
                if let Some(binop) = BinOp::from_str(&op) {
                    if binop == BinOp::Add || binop == BinOp::Sub {
                        self.next();
                        let right = self.parse_multiplicative_expr()?;
                        left = Expr::binary(left, binop, right);
                        continue;
                    }
                }
                // String concatenation ||
                if op == "||" {
                    self.next();
                    let right = self.parse_multiplicative_expr()?;
                    left = Expr::binary(left, BinOp::Concat, right);
                    continue;
                }
            }
            break;
        }
        Ok(left)
    }

    fn parse_multiplicative_expr(&mut self) -> Result<Expr, String> {
        let mut left = self.parse_unary_expr()?;
        loop {
            if let Token::Op(op) = self.peek().clone() {
                if let Some(binop) = BinOp::from_str(&op) {
                    if binop == BinOp::Mul || binop == BinOp::Div || binop == BinOp::Mod {
                        self.next();
                        let right = self.parse_unary_expr()?;
                        left = Expr::binary(left, binop, right);
                        continue;
                    }
                }
            }
            break;
        }
        Ok(left)
    }

    /// Parse a unary prefix expression: `-expr`, `+expr`. Falls through
    /// to `parse_primary` if no unary operator is present.
    fn parse_unary_expr(&mut self) -> Result<Expr, String> {
        if let Token::Op(op) = self.peek().clone() {
            if op == "-" {
                self.next();
                let inner = self.parse_unary_expr()?;
                return Ok(Expr::Unary { op: UnaryOp::Neg, expr: Box::new(inner) });
            }
            if op == "+" {
                self.next();
                let inner = self.parse_unary_expr()?;
                return Ok(Expr::Unary { op: UnaryOp::Pos, expr: Box::new(inner) });
            }
        }
        self.parse_primary()
    }

    fn parse_primary(&mut self) -> Result<Expr, String> {
        match self.peek().clone() {
            Token::Int(i) => {
                self.next();
                Ok(Expr::Literal(Value::Int(i)))
            }
            Token::Float(f) => {
                self.next();
                Ok(Expr::Literal(Value::Float(f)))
            }
            Token::String(s) => {
                self.next();
                Ok(Expr::Literal(Value::String(s)))
            }
            Token::Hex(h) => {
                self.next();
                Ok(Expr::Literal(Value::Hex(h)))
            }
            Token::Keyword(kw) => {
                let kw_upper = kw.to_uppercase();
                if kw_upper == "DATE" {
                    self.next();
                    if let Token::String(s) = self.peek().clone() {
                        self.next();
                        if let Ok(d) = crate::types::Date::from_str(&s) {
                            return Ok(Expr::Literal(Value::Date(d.0)));
                        }
                        return Ok(Expr::Literal(Value::String(s)));
                    }
                    return Ok(Expr::Column(kw));
                }
                if kw_upper == "NULL" {
                    self.next();
                    return Ok(Expr::Literal(Value::Null));
                }
                // Wave 60a: CASE WHEN expression.
                if kw_upper == "CASE" {
                    return self.parse_case();
                }
                // Wave 67: EXTRACT(field FROM expr).
                if kw_upper == "EXTRACT" {
                    return self.parse_extract();
                }
                // Wave 67: CAST(expr AS type).
                if kw_upper == "CAST" {
                    return self.parse_cast();
                }
                // Other keywords treated as identifiers
                self.next();
                Ok(Expr::Column(kw))
            }
            Token::Ident(name) => {
                self.next();
                // Wave 62 fix: if the next token is '(', this is a function
                // call (e.g. count(*), sum(col), avg(col)). Parse it as
                // Expr::Function so HAVING expressions can reference aggregates.
                if matches!(self.peek(), Token::LParen) {
                    self.next(); // consume (
                    let distinct = self.match_ident("DISTINCT");
                    let mut args: Vec<Expr> = Vec::new();
                    // COUNT(*) — single wildcard arg.
                    if self.match_op("*") {
                        args.push(Expr::Wildcard);
                    } else if !matches!(self.peek(), Token::RParen) {
                        // Parse comma-separated argument list.
                        loop {
                            args.push(self.parse_expr()?);
                            match self.peek() {
                                Token::Comma => {
                                    self.next();
                                }
                                Token::RParen => break,
                                other => {
                                    return Err(format!(
                                        "expected , or ) in function args, got {other:?}"
                                    ));
                                }
                            }
                        }
                    }
                    if !matches!(self.peek(), Token::RParen) {
                        return Err(format!(
                            "expected ) after function args, got {:?}",
                            self.peek()
                        ));
                    }
                    self.next(); // consume )
                    return Ok(Expr::Function {
                        name: name.to_uppercase(),
                        args,
                        distinct,
                    });
                }
                // Qualified column reference: t.col or t.*
                if let Token::Op(op) = self.peek() {
                    if op == "." {
                        self.next(); // consume .
                        let second = match self.peek().clone() {
                            Token::Ident(s) => s,
                            Token::Keyword(k) => k,
                            Token::Op(o) if o == "*" => {
                                // t.* — qualified wildcard. Stored as a
                                // Column with the dotted name so the
                                // executor can detect the trailing `.*`.
                                return Ok(Expr::Column(format!("{name}.*")));
                            }
                            other => {
                                return Err(format!(
                                    "expected name after '.', got {other:?}"
                                ));
                            }
                        };
                        self.next();
                        return Ok(Expr::Column(format!("{name}.{second}")));
                    }
                }
                Ok(Expr::Column(name))
            }
            Token::LParen => {
                self.next();
                // Scalar subquery: ( SELECT ... ) — capture as a subquery SQL
                // string for the executor to re-parse. Scalar subqueries
                // return a single value used in expression context.
                if matches!(self.peek(), Token::Keyword(k) if k == "SELECT") {
                    let subquery_sql = self.capture_subquery_sql()?;
                    if !matches!(self.peek(), Token::RParen) {
                        return Err(format!(
                            "expected ) after scalar subquery, got {:?}",
                            self.peek()
                        ));
                    }
                    self.next(); // consume )
                    // Store as an InSubquery-like variant? No — scalar
                    // subqueries are values, not predicates. We reuse the
                    // subquery SQL string and wrap in a synthetic Function
                    // call so the executor can detect it. This is a
                    // simplification; a proper ScalarSubquery variant
                    // would be cleaner but requires more downstream changes.
                    return Ok(Expr::Function {
                        name: "__scalar_subquery__".to_string(),
                        args: vec![Expr::Literal(Value::String(subquery_sql))],
                        distinct: false,
                    });
                }
                let e = self.parse_expr()?;
                if !matches!(self.peek(), Token::RParen) {
                    return Err(format!("expected ), got {:?}", self.peek()));
                }
                self.next();
                Ok(Expr::Paren(Box::new(e)))
            }
            other => Err(format!("expected expression, got {other:?}")),
        }
    }

    /// Parse a CASE WHEN expression (Wave 60a).
    /// Syntax: `CASE WHEN <cond> THEN <result> [WHEN ... THEN ...] [ELSE <result>] END`
    fn parse_case(&mut self) -> Result<Expr, String> {
        self.expect_keyword("CASE")?;
        let mut when_clauses: Vec<(Expr, Expr)> = Vec::new();
        while self.match_keyword("WHEN") {
            let cond = self.parse_expr()?;
            self.expect_keyword("THEN")?;
            let result = self.parse_expr()?;
            when_clauses.push((cond, result));
        }
        if when_clauses.is_empty() {
            return Err("CASE expression must have at least one WHEN clause".into());
        }
        let else_clause =
            if self.match_keyword("ELSE") { Some(Box::new(self.parse_expr()?)) } else { None };
        self.expect_keyword("END")?;
        Ok(Expr::Case { when_clauses, else_clause })
    }

    /// Parse `EXTRACT(field FROM expr)` (Wave 67).
    ///
    /// Syntax: `EXTRACT ( YEAR FROM date_col )`, `EXTRACT(MONTH FROM col)`, etc.
    /// The field is an identifier or keyword (YEAR, MONTH, DAY, HOUR, MINUTE,
    /// SECOND). The expr can be any expression (typically a column reference
    /// or a DATE literal).
    fn parse_extract(&mut self) -> Result<Expr, String> {
        self.expect_keyword("EXTRACT")?;
        if !matches!(self.peek(), Token::LParen) {
            return Err(format!("expected ( after EXTRACT, got {:?}", self.peek()));
        }
        self.next(); // consume (
                     // The field is a keyword (YEAR, MONTH, DAY, ...) or an identifier.
        let field = match self.peek().clone() {
            Token::Keyword(k) => k.to_uppercase(),
            Token::Ident(s) => s.to_uppercase(),
            other => return Err(format!("expected field name in EXTRACT, got {other:?}")),
        };
        self.next();
        self.expect_keyword("FROM")?;
        let expr = self.parse_expr()?;
        if !matches!(self.peek(), Token::RParen) {
            return Err(format!("expected ) after EXTRACT expr, got {:?}", self.peek()));
        }
        self.next(); // consume )
        Ok(Expr::Extract { field, expr: Box::new(expr) })
    }

    /// Parse `CAST(expr AS target_type)` (Wave 67).
    ///
    /// Syntax: `CAST ( col AS INT )`, `CAST(col AS FLOAT)`, `CAST(col AS VARCHAR)`,
    /// `CAST(col AS BIGINT)`. The target_type is a type keyword; the optional
    /// `(length)` for VARCHAR is consumed but ignored.
    fn parse_cast(&mut self) -> Result<Expr, String> {
        self.expect_keyword("CAST")?;
        if !matches!(self.peek(), Token::LParen) {
            return Err(format!("expected ( after CAST, got {:?}", self.peek()));
        }
        self.next(); // consume (
        let expr = self.parse_expr()?;
        self.expect_keyword("AS")?;
        // The target type is a keyword (INT, FLOAT, VARCHAR, BIGINT, etc.)
        // or an identifier.
        let target_type = match self.peek().clone() {
            Token::Keyword(k) => k.to_uppercase(),
            Token::Ident(s) => s.to_uppercase(),
            other => return Err(format!("expected target type in CAST, got {other:?}")),
        };
        self.next();
        // Optional (length) for VARCHAR(n) — consume and ignore.
        if matches!(self.peek(), Token::LParen) {
            self.next();
            while !matches!(self.peek(), Token::RParen | Token::EOF) {
                self.next();
            }
            if matches!(self.peek(), Token::RParen) {
                self.next();
            }
        }
        if !matches!(self.peek(), Token::RParen) {
            return Err(format!("expected ) after CAST expr, got {:?}", self.peek()));
        }
        self.next(); // consume )
        Ok(Expr::Cast { expr: Box::new(expr), target_type })
    }
}

// -----------------------------------------------------------------------
// Wave 7 (Task 7.2): Formal MERGE AST + parser.
//
// Replaces the `parse_merge` string-scan hack that lived in
// `src/engine/helpers.rs`. The formal parser produces a `MergeStmt` AST
// which is then converted to `crate::exec::merge::Merge` for execution.
// -----------------------------------------------------------------------

/// Formal AST for a MERGE statement (Wave 7 — replaces the `parse_merge` hack).
///
/// Represents the parsed structure of:
/// ```sql
/// MERGE [INTO] <target> [AS <alias>]
/// USING (VALUES (...), (...)) AS <source_alias> (<col1>, <col2>, ...)
///   ON <target_alias>.<col> = <source_alias>.<col>
///   [WHEN MATCHED [THEN] UPDATE SET <col> = <val>, ...]
///   [WHEN NOT MATCHED [BY TARGET] [THEN] INSERT (<cols>) VALUES (<vals>)]
/// ```
///
/// The `to_merge` conversion (defined in `engine::helpers`) bridges this
/// AST to the existing `exec::merge::Merge` executor struct.
#[derive(Debug, Clone)]
pub struct MergeStmt {
    /// Target table name (the table to merge INTO).
    pub target: String,
    /// Source of rows to merge.
    pub source: MergeSource,
    /// Join condition: `target.col = source.col` (aliases stripped to just
    /// the column names).
    pub on: MergeOn,
    /// Actions for matched rows (target row has a matching source row).
    /// The existing `exec::merge::Merge` executor only consumes the first
    /// action; subsequent actions are silently dropped (TODO: multi-action).
    pub when_matched: Vec<MergeAction>,
    /// Actions for not-matched rows (source row has no matching target).
    pub when_not_matched: Vec<MergeAction>,
}

/// The source of rows in a MERGE statement.
#[derive(Debug, Clone)]
pub enum MergeSource {
    /// `USING <table_name>` — references a catalog table.
    /// (Not yet wired through to execution; left for a future wave.)
    Table(String),
    /// `USING (VALUES (...), (...)) AS <alias> (<col1>, <col2>, ...)` —
    /// an inline values list with column names.
    Values {
        /// Raw rows from the VALUES list. Each row is a Vec of stringified
        /// cell values (in `col_names` order).
        rows: Vec<Vec<String>>,
        /// Column names (parallel to each row in `rows`).
        col_names: Vec<String>,
    },
    /// `USING (SELECT ...)` — a subquery (stored as a best-effort SQL string).
    /// (Not yet wired through to execution; left for a future wave.)
    Subquery(String),
}

/// Join condition for MERGE: `target.col = source.col`.
#[derive(Debug, Clone)]
pub struct MergeOn {
    /// The column on the target side (alias stripped).
    pub target_col: String,
    /// The column on the source side (alias stripped).
    pub source_col: String,
}

/// A single MERGE action.
#[derive(Debug, Clone)]
pub enum MergeAction {
    /// `UPDATE SET col = val, ...`
    Update {
        /// List of (column, value) pairs. The value is preserved as a raw
        /// string (e.g. `source.val`, `99`, `'hello'`) so the executor can
        /// resolve `source.col` references against the current source row.
        sets: Vec<(String, String)>,
    },
    /// `DELETE`
    Delete,
    /// `INSERT (cols) VALUES (vals)`
    Insert {
        /// Column names to insert into.
        columns: Vec<String>,
        /// Values to insert (parallel to `columns`).
        values: Vec<String>,
    },
}

/// Parse a `MERGE` statement into a [`MergeStmt`] AST.
///
/// Supported syntax (SQL Server / Snowflake style):
/// ```sql
/// MERGE [INTO] <target> [AS <alias>]
/// USING (VALUES (...), (...)) AS <source_alias> (<col1>, <col2>, ...)
///   ON <target_alias>.<col> = <source_alias>.<col>
///   [WHEN MATCHED [THEN] UPDATE SET <col> = <val>, ...]
///   [WHEN NOT MATCHED [BY TARGET] [THEN] INSERT (<cols>) VALUES (<vals>)]
/// ```
///
/// The parser is token-based (not string-scan), producing a formal AST
/// that is then converted to `crate::exec::merge::Merge` for execution.
///
/// # Errors
///
/// Returns `Err(String)` for malformed MERGE statements (missing USING,
/// missing ON, unbalanced parens, unknown action verb, etc.).
pub fn parse_merge_stmt(tokens: Vec<Token>) -> Result<MergeStmt, String> {
    let mut p = Parser::new(tokens);
    // MERGE is not in the KEYWORDS list, so it tokenizes as `Ident("MERGE")`
    // (or `Ident("merge")`). `match_ident` does case-insensitive matching
    // against both Ident and Keyword tokens, so we use it for MERGE and
    // the other non-keyword tokens (MATCHED, TARGET). Standard SQL keywords
    // (USING, ON, WHEN, BY, THEN, UPDATE, SET, DELETE, INSERT, VALUES) use
    // `match_keyword` since they're in the lexer's KEYWORDS list.
    if !p.match_ident("MERGE") {
        return Err(format!("expected MERGE, got {:?}", p.peek()));
    }
    let _ = p.match_ident("INTO");
    let target = p.parse_table_name()?;
    // Optional `AS alias` on target.
    let _ = p.parse_optional_alias()?;
    if !p.match_keyword("USING") {
        return Err(format!("expected USING, got {:?}", p.peek()));
    }
    let source = parse_merge_source(&mut p)?;
    if !p.match_keyword("ON") {
        return Err(format!("expected ON, got {:?}", p.peek()));
    }
    let on = parse_merge_on(&mut p)?;
    let mut when_matched = Vec::new();
    let mut when_not_matched = Vec::new();
    while p.match_keyword("WHEN") {
        if p.match_ident("MATCHED") {
            // Optional `AND <pred>` — we don't support predicates yet; skip
            // the rest of the clause if AND is present (TODO).
            let _ = p.match_keyword("THEN");
            let action = parse_merge_action(&mut p)?;
            when_matched.push(action);
        } else if p.match_keyword("NOT") {
            if !p.match_ident("MATCHED") {
                return Err(format!("expected MATCHED after NOT, got {:?}", p.peek()));
            }
            // Optional `BY TARGET` (the default).
            let _ = p.match_keyword("BY");
            let _ = p.match_ident("TARGET");
            let _ = p.match_keyword("THEN");
            let action = parse_merge_action(&mut p)?;
            when_not_matched.push(action);
        } else {
            return Err(format!(
                "expected MATCHED or NOT MATCHED in WHEN clause, got {:?}",
                p.peek()
            ));
        }
    }
    match p.peek() {
        Token::Semicolon | Token::EOF => {}
        other => return Err(format!("unexpected trailing token in MERGE: {other:?}")),
    }
    Ok(MergeStmt { target, source, on, when_matched, when_not_matched })
}

/// Parse the USING clause: either `(VALUES ...) AS alias (cols)` or a
/// table name. Returns a [`MergeSource`].
fn parse_merge_source(p: &mut Parser) -> Result<MergeSource, String> {
    if matches!(p.peek(), Token::LParen) {
        // Could be (VALUES ...) or (SELECT ...).
        p.next(); // consume (
        if p.match_keyword("VALUES") {
            let mut rows = Vec::new();
            loop {
                if !matches!(p.peek(), Token::LParen) {
                    break;
                }
                p.next(); // consume (
                let mut row = Vec::new();
                loop {
                    let val = parse_value_expr(p)?;
                    row.push(val);
                    if matches!(p.peek(), Token::Comma) {
                        p.next();
                        continue;
                    }
                    break;
                }
                if !matches!(p.peek(), Token::RParen) {
                    return Err(format!("expected ) after VALUES row, got {:?}", p.peek()));
                }
                p.next(); // consume )
                rows.push(row);
                if matches!(p.peek(), Token::Comma) {
                    p.next();
                    continue;
                }
                break;
            }
            if !matches!(p.peek(), Token::RParen) {
                return Err(format!(
                    "expected ) to close USING (VALUES ...), got {:?}",
                    p.peek()
                ));
            }
            p.next(); // consume ) — closes the outer USING (...) group.
            // Optional `AS alias (col1, col2, ...)`.
            let _ = p.match_keyword("AS");
            // Skip the alias identifier if present (and not a SQL keyword
            // like ON / WHEN — those Keywords aren't consumed by `next`).
            if let Token::Ident(_) = p.peek() {
                let _ = p.next();
            }
            let mut col_names = Vec::new();
            if matches!(p.peek(), Token::LParen) {
                p.next(); // consume (
                loop {
                    let col = match p.peek().clone() {
                        Token::Ident(s) => s,
                        other => {
                            return Err(format!(
                                "expected column name in USING (cols), got {other:?}"
                            ))
                        }
                    };
                    p.next();
                    col_names.push(col);
                    if matches!(p.peek(), Token::Comma) {
                        p.next();
                        continue;
                    }
                    break;
                }
                if !matches!(p.peek(), Token::RParen) {
                    return Err(format!("expected ) after USING (cols), got {:?}", p.peek()));
                }
                p.next(); // consume )
            }
            Ok(MergeSource::Values { rows, col_names })
        } else {
            // Subquery: (SELECT ...). Collect tokens until matching ).
            let mut depth = 1i32;
            let mut tokens: Vec<Token> = Vec::new();
            while depth > 0 {
                match p.next() {
                    Token::LParen => {
                        depth += 1;
                        tokens.push(Token::LParen);
                    }
                    Token::RParen => {
                        depth -= 1;
                        if depth > 0 {
                            tokens.push(Token::RParen);
                        }
                    }
                    Token::EOF => return Err("unterminated subquery in MERGE USING".into()),
                    other => tokens.push(other),
                }
            }
            // Best-effort: re-stringify the captured tokens.
            let sql = tokens.iter().map(|t| format!("{t:?}")).collect::<Vec<_>>().join(" ");
            Ok(MergeSource::Subquery(sql))
        }
    } else {
        // Table name source.
        let name = p.parse_table_name()?;
        Ok(MergeSource::Table(name))
    }
}

/// Parse the ON clause: `<alias>.<col> = <alias>.<col>`. Aliases are
/// stripped; only the column names are kept.
fn parse_merge_on(p: &mut Parser) -> Result<MergeOn, String> {
    let lhs = parse_qualified_col(p)?;
    if !p.match_op("=") {
        return Err(format!("expected = in ON clause, got {:?}", p.peek()));
    }
    let rhs = parse_qualified_col(p)?;
    Ok(MergeOn {
        target_col: lhs.col,
        source_col: rhs.col,
    })
}

/// A parsed `<alias>.<col>` reference (used in MERGE ON clauses and SET
/// assignments). The alias is kept for diagnostics but discarded by the
/// caller (the executor only needs the column name).
struct QualifiedCol {
    #[allow(dead_code)]
    alias: Option<String>,
    col: String,
}

/// Parse a qualified column reference `<alias>.<col>` (or a bare `<col>`).
/// Returns the alias (if any) and the column name.
fn parse_qualified_col(p: &mut Parser) -> Result<QualifiedCol, String> {
    let name = match p.peek().clone() {
        Token::Ident(s) => s,
        other => return Err(format!("expected column reference, got {other:?}")),
    };
    p.next();
    if matches!(p.peek(), Token::Op(o) if o == ".") {
        p.next(); // consume .
        let col = match p.peek().clone() {
            Token::Ident(s) => s,
            other => return Err(format!("expected column name after '.', got {other:?}")),
        };
        p.next();
        Ok(QualifiedCol { alias: Some(name), col })
    } else {
        Ok(QualifiedCol { alias: None, col: name })
    }
}

/// Parse a MERGE action: `UPDATE SET ...`, `DELETE`, or
/// `INSERT (...) VALUES (...)`.
fn parse_merge_action(p: &mut Parser) -> Result<MergeAction, String> {
    if p.match_keyword("UPDATE") {
        if !p.match_keyword("SET") {
            return Err(format!("expected SET after UPDATE, got {:?}", p.peek()));
        }
        let mut sets = Vec::new();
        loop {
            let col_qualified = parse_qualified_col(p)?;
            let col = col_qualified.col; // strip alias from LHS
            if !p.match_op("=") {
                return Err(format!("expected = in SET assignment, got {:?}", p.peek()));
            }
            let val = parse_value_expr(p)?;
            sets.push((col, val));
            if matches!(p.peek(), Token::Comma) {
                p.next();
                continue;
            }
            break;
        }
        Ok(MergeAction::Update { sets })
    } else if p.match_keyword("DELETE") {
        Ok(MergeAction::Delete)
    } else if p.match_keyword("INSERT") {
        let mut columns = Vec::new();
        if matches!(p.peek(), Token::LParen) {
            p.next(); // consume (
            loop {
                let col = parse_qualified_col(p)?;
                columns.push(col.col);
                if matches!(p.peek(), Token::Comma) {
                    p.next();
                    continue;
                }
                break;
            }
            if !matches!(p.peek(), Token::RParen) {
                return Err(format!("expected ) after INSERT columns, got {:?}", p.peek()));
            }
            p.next(); // consume )
        }
        let mut values = Vec::new();
        if p.match_keyword("VALUES") {
            if !matches!(p.peek(), Token::LParen) {
                return Err(format!("expected ( after VALUES in INSERT, got {:?}", p.peek()));
            }
            p.next(); // consume (
            loop {
                let val = parse_value_expr(p)?;
                values.push(val);
                if matches!(p.peek(), Token::Comma) {
                    p.next();
                    continue;
                }
                break;
            }
            if !matches!(p.peek(), Token::RParen) {
                return Err(format!("expected ) after VALUES row, got {:?}", p.peek()));
            }
            p.next(); // consume )
        }
        Ok(MergeAction::Insert { columns, values })
    } else {
        Err(format!(
            "expected UPDATE/DELETE/INSERT in WHEN clause, got {:?}",
            p.peek()
        ))
    }
}

/// Parse a single value expression: a literal (int/float/string/hex) or
/// a (possibly qualified) column reference. Returns the stringified form
/// (e.g. `42`, `'hello'`, `source.val`) preserving any column references
/// for the executor to resolve.
fn parse_value_expr(p: &mut Parser) -> Result<String, String> {
    let mut prefix = String::new();
    if matches!(p.peek(), Token::Op(o) if o == "-") {
        prefix.push('-');
        p.next();
    } else if matches!(p.peek(), Token::Op(o) if o == "+") {
        let _ = p.next();
    }
    match p.peek().clone() {
        Token::Int(i) => {
            p.next();
            Ok(format!("{prefix}{i}"))
        }
        Token::Float(f) => {
            p.next();
            Ok(format!("{prefix}{f}"))
        }
        Token::String(s) => {
            p.next();
            Ok(format!("{prefix}'{s}'"))
        }
        Token::Hex(bytes) => {
            p.next();
            let hex: String = bytes.iter().map(|b| format!("{:02X}", b)).collect();
            Ok(format!("{prefix}x'{hex}'"))
        }
        Token::Ident(name) => {
            p.next();
            if matches!(p.peek(), Token::Op(o) if o == ".") {
                p.next();
                match p.peek().clone() {
                    Token::Ident(col) => {
                        p.next();
                        Ok(format!("{prefix}{name}.{col}"))
                    }
                    other => Err(format!("expected column name after '.', got {other:?}")),
                }
            } else {
                Ok(format!("{prefix}{name}"))
            }
        }
        other => Err(format!("expected value in MERGE action, got {other:?}")),
    }
}

#[cfg(test)]
#[path = "parser_tests.rs"]
mod tests;
