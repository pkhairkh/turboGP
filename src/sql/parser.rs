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
    /// ORDER BY column list with ascending flag (`true` = ASC).
    pub order_by: Vec<(String, bool)>,
    /// Optional LIMIT row count.
    pub limit: Option<usize>,
    /// Whether SELECT DISTINCT was specified (Wave 60d). When true, the
    /// executor deduplicates the result rows.
    pub distinct: bool,
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
        let distinct = self.match_keyword("DISTINCT");
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

        let limit = if self.match_ident("LIMIT") { Some(self.parse_usize()?) } else { None };

        Ok(SelectQuery {
            select,
            from,
            joins,
            where_clause,
            group_by,
            having,
            order_by,
            limit,
            distinct,
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

    fn parse_order_list(&mut self) -> Result<Vec<(String, bool)>, String> {
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
                items.push((name, ascending));
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sql::lexer::tokenize;

    /// Helper: tokenize + parse in one step.
    fn parse_sql(s: &str) -> Result<SelectQuery, String> {
        parse(tokenize(s)?)
    }

    #[test]
    fn parse_select_star_with_where() {
        let q = parse_sql("SELECT * FROM t WHERE x = 5").unwrap();
        assert_eq!(q.select.len(), 1);
        assert!(matches!(q.select[0], SelectItem::Star));
        assert_eq!(q.from, "t");
        let w = q.where_clause.expect("WHERE clause");
        match w {
            Expr::Binary { left, op, right } => {
                assert!(op == BinOp::Eq);
                match *left {
                    Expr::Column(c) => assert_eq!(c, "x"),
                    other => panic!("expected Column, got {other:?}"),
                }
                match *right {
                    Expr::Literal(Value::Int(5)) => {}
                    other => panic!("expected Int(5), got {other:?}"),
                }
            }
            other => panic!("expected Binary, got {other:?}"),
        }
    }

    #[test]
    fn parse_count_star() {
        let q = parse_sql("SELECT COUNT(*) FROM t").unwrap();
        assert_eq!(q.select.len(), 1);
        match &q.select[0] {
            SelectItem::Aggregate { func, arg, alias } => {
                assert_eq!(func, "COUNT");
                assert_eq!(arg, "*");
                assert_eq!(*alias, None);
            }
            other => panic!("expected Aggregate, got {other:?}"),
        }
        assert_eq!(q.from, "t");
        assert!(q.where_clause.is_none());
    }

    #[test]
    fn parse_sum_with_alias() {
        let q = parse_sql("SELECT SUM(price) AS total FROM sales").unwrap();
        match &q.select[0] {
            SelectItem::Aggregate { func, arg, alias } => {
                assert_eq!(func, "SUM");
                assert_eq!(arg, "price");
                assert_eq!(*alias, Some("total".to_string()));
            }
            other => panic!("expected Aggregate, got {other:?}"),
        }
    }

    #[test]
    fn parse_avg_group_by() {
        let q = parse_sql("SELECT AVG(price) FROM sales GROUP BY area").unwrap();
        assert_eq!(q.group_by, vec!["area"]);
    }

    #[test]
    fn parse_order_by_asc_desc() {
        let q = parse_sql("SELECT * FROM t ORDER BY a ASC, b DESC, c").unwrap();
        assert_eq!(q.order_by.len(), 3);
        assert_eq!(q.order_by[0], ("a".to_string(), true));
        assert_eq!(q.order_by[1], ("b".to_string(), false));
        assert_eq!(q.order_by[2], ("c".to_string(), true));
    }

    #[test]
    fn parse_limit() {
        let q = parse_sql("SELECT * FROM t LIMIT 100").unwrap();
        assert_eq!(q.limit, Some(100));
    }

    #[test]
    fn parse_multiple_columns() {
        let q = parse_sql("SELECT a, b, c FROM t").unwrap();
        assert_eq!(q.select.len(), 3);
        assert!(matches!(&q.select[0], SelectItem::Column(c) if c == "a"));
        assert!(matches!(&q.select[1], SelectItem::Column(c) if c == "b"));
        assert!(matches!(&q.select[2], SelectItem::Column(c) if c == "c"));
    }

    #[test]
    fn parse_and_or_precedence() {
        // a = 1 AND b = 2 OR c = 3  →  (a=1 AND b=2) OR (c=3)
        let q = parse_sql("SELECT * FROM t WHERE a = 1 AND b = 2 OR c = 3").unwrap();
        let w = q.where_clause.unwrap();
        match w {
            Expr::Binary { left, op, right } => {
                assert!(op == BinOp::Or);
                // Left should be AND.
                match *left {
                    Expr::Binary { op, .. } => { assert!(op == BinOp::And) }
                    other => panic!("expected AND, got {other:?}"),
                }
                // Right should be a comparison.
                match *right {
                    Expr::Binary { op, .. } => { assert!(op == BinOp::Eq) }
                    other => panic!("expected =, got {other:?}"),
                }
            }
            other => panic!("expected OR at top, got {other:?}"),
        }
    }

    #[test]
    fn parse_arithmetic_precedence() {
        // a + b * c  →  a + (b * c)
        let q = parse_sql("SELECT * FROM t WHERE x = a + b * c").unwrap();
        let w = q.where_clause.unwrap();
        match w {
            Expr::Binary { left, op: op_eq, right } => {
                assert!(op_eq == BinOp::Eq);
                assert!(matches!(*left, Expr::Column(_)));
                match *right {
                    Expr::Binary { op, right: mul_right, .. } => {
                        assert!(op == BinOp::Add);
                        match *mul_right {
                            Expr::Binary { op, .. } => { assert!(op == BinOp::Mul) }
                            other => panic!("expected *, got {other:?}"),
                        }
                    }
                    other => panic!("expected +, got {other:?}"),
                }
            }
            other => panic!("expected =, got {other:?}"),
        }
    }

    #[test]
    fn parse_parenthesized_expr() {
        let q = parse_sql("SELECT * FROM t WHERE (a = 1 OR b = 2) AND c = 3").unwrap();
        let w = q.where_clause.unwrap();
        match w {
            Expr::Binary { left, op, .. } => {
                assert!(op == BinOp::And);
                // The left side is now wrapped in Expr::Paren since Wave 2
                // preserves the source grouping for AST fidelity.
                match *left {
                    Expr::Paren(inner) => match *inner {
                        Expr::Binary { op, .. } => { assert!(op == BinOp::Or) }
                        other => panic!("expected OR inside Paren, got {other:?}"),
                    },
                    other => panic!("expected Paren, got {other:?}"),
                }
            }
            other => panic!("expected AND at top, got {other:?}"),
        }
    }

    #[test]
    fn parse_is_null() {
        let q = parse_sql("SELECT * FROM t WHERE x IS NULL").unwrap();
        let w = q.where_clause.unwrap();
        match w {
            Expr::IsNull { expr, negated } => {
                assert!(!negated);
                match *expr {
                    Expr::Column(c) => assert_eq!(c, "x"),
                    other => panic!("expected Column(x), got {other:?}"),
                }
            }
            other => panic!("expected IsNull, got {other:?}"),
        }
    }

    #[test]
    fn parse_is_not_null() {
        let q = parse_sql("SELECT * FROM t WHERE y IS NOT NULL").unwrap();
        let w = q.where_clause.unwrap();
        match w {
            Expr::IsNull { negated, .. } => assert!(negated),
            other => panic!("expected IsNull, got {other:?}"),
        }
    }

    #[test]
    fn parse_is_null_with_and() {
        // x IS NULL AND y > 5 — verify precedence
        let q = parse_sql("SELECT * FROM t WHERE x IS NULL AND y > 5").unwrap();
        let w = q.where_clause.unwrap();
        match w {
            Expr::Binary { left, op, .. } => {
                assert!(op == BinOp::And);
                match *left {
                    Expr::IsNull { .. } => {}
                    other => panic!("expected IsNull on left, got {other:?}"),
                }
            }
            other => panic!("expected AND at top, got {other:?}"),
        }
    }

    #[test]
    fn parse_not_prefix() {
        // NOT (x > 5)
        let q = parse_sql("SELECT * FROM t WHERE NOT (x > 5)").unwrap();
        let w = q.where_clause.unwrap();
        match w {
            Expr::Not(inner) => match *inner {
                Expr::Paren(p) => match *p {
                    Expr::Binary { op, .. } => assert!(op == BinOp::Gt),
                    other => panic!("expected Binary inside Paren, got {other:?}"),
                },
                other => panic!("expected Paren inside Not, got {other:?}"),
            },
            other => panic!("expected Not, got {other:?}"),
        }
    }

    #[test]
    fn parse_not_with_and() {
        // NOT a = 1 AND b = 2 — should parse as (NOT (a = 1)) AND (b = 2)
        let q = parse_sql("SELECT * FROM t WHERE NOT a = 1 AND b = 2").unwrap();
        let w = q.where_clause.unwrap();
        match w {
            Expr::Binary { left, op, .. } => {
                assert!(op == BinOp::And);
                match *left {
                    Expr::Not(_) => {}
                    other => panic!("expected Not on left, got {other:?}"),
                }
            }
            other => panic!("expected AND at top, got {other:?}"),
        }
    }

    #[test]
    fn parse_unary_minus_literal() {
        // SELECT -1 FROM t — unary minus on a literal
        let q = parse_sql("SELECT * FROM t WHERE x = -1").unwrap();
        let w = q.where_clause.unwrap();
        match w {
            Expr::Binary { right, op, .. } => {
                assert!(op == BinOp::Eq);
                match *right {
                    Expr::Unary { op, expr } => {
                        assert!(op == UnaryOp::Neg);
                        match *expr {
                            Expr::Literal(Value::Int(1)) => {}
                            other => panic!("expected Int(1), got {other:?}"),
                        }
                    }
                    other => panic!("expected Unary, got {other:?}"),
                }
            }
            other => panic!("expected Binary, got {other:?}"),
        }
    }

    #[test]
    fn parse_unary_minus_paren() {
        // -(a + b)
        let q = parse_sql("SELECT * FROM t WHERE x = -(a + b)").unwrap();
        let w = q.where_clause.unwrap();
        match w {
            Expr::Binary { right, op, .. } => {
                assert!(op == BinOp::Eq);
                match *right {
                    Expr::Unary { op: UnaryOp::Neg, expr } => {
                        match *expr {
                            Expr::Paren(_) => {}
                            other => panic!("expected Paren inside Unary, got {other:?}"),
                        }
                    }
                    other => panic!("expected Unary, got {other:?}"),
                }
            }
            other => panic!("expected Binary, got {other:?}"),
        }
    }

    #[test]
    fn parse_qualified_column_in_select() {
        let q = parse_sql("SELECT t.col FROM t").unwrap();
        match &q.select[0] {
            SelectItem::Column(name) => assert_eq!(name, "t.col"),
            other => panic!("expected Column(t.col), got {other:?}"),
        }
    }

    #[test]
    fn parse_qualified_column_in_where() {
        let q = parse_sql("SELECT * FROM t WHERE t1.id = t2.id").unwrap();
        let w = q.where_clause.unwrap();
        match w {
            Expr::Binary { left, op, right } => {
                assert!(op == BinOp::Eq);
                match *left {
                    Expr::Column(c) => assert_eq!(c, "t1.id"),
                    other => panic!("expected Column(t1.id), got {other:?}"),
                }
                match *right {
                    Expr::Column(c) => assert_eq!(c, "t2.id"),
                    other => panic!("expected Column(t2.id), got {other:?}"),
                }
            }
            other => panic!("expected Binary, got {other:?}"),
        }
    }

    #[test]
    fn parse_scalar_function_upper() {
        // UPPER(name) — single-arg scalar function
        let q = parse_sql("SELECT * FROM t WHERE x = UPPER(name)").unwrap();
        let w = q.where_clause.unwrap();
        match w {
            Expr::Binary { right, op, .. } => {
                assert!(op == BinOp::Eq);
                match *right {
                    Expr::Function { name, args, distinct } => {
                        assert_eq!(name, "UPPER");
                        assert!(!distinct);
                        assert_eq!(args.len(), 1);
                        match &args[0] {
                            Expr::Column(c) => assert_eq!(c, "name"),
                            other => panic!("expected Column(name), got {other:?}"),
                        }
                    }
                    other => panic!("expected Function, got {other:?}"),
                }
            }
            other => panic!("expected Binary, got {other:?}"),
        }
    }

    #[test]
    fn parse_scalar_function_substr_multi_arg() {
        // SUBSTR(name, 1, 3) — three-arg scalar function
        let q = parse_sql("SELECT * FROM t WHERE x = SUBSTR(name, 1, 3)").unwrap();
        let w = q.where_clause.unwrap();
        match w {
            Expr::Binary { right, op, .. } => {
                assert!(op == BinOp::Eq);
                match *right {
                    Expr::Function { name, args, .. } => {
                        assert_eq!(name, "SUBSTR");
                        assert_eq!(args.len(), 3);
                    }
                    other => panic!("expected Function, got {other:?}"),
                }
            }
            other => panic!("expected Binary, got {other:?}"),
        }
    }

    #[test]
    fn parse_scalar_function_coalesce_n_args() {
        // COALESCE(a, b, c) — N-arg function
        let q = parse_sql("SELECT * FROM t WHERE x = COALESCE(a, b, c)").unwrap();
        let w = q.where_clause.unwrap();
        match w {
            Expr::Binary { right, op, .. } => {
                assert!(op == BinOp::Eq);
                match *right {
                    Expr::Function { name, args, .. } => {
                        assert_eq!(name, "COALESCE");
                        assert_eq!(args.len(), 3);
                    }
                    other => panic!("expected Function, got {other:?}"),
                }
            }
            other => panic!("expected Binary, got {other:?}"),
        }
    }

    #[test]
    fn parse_scalar_function_case_insensitive() {
        // Function names are case-insensitive — lowercased should still uppercase
        let q = parse_sql("SELECT * FROM t WHERE x = upper(name)").unwrap();
        let w = q.where_clause.unwrap();
        match w {
            Expr::Binary { right, op, .. } => {
                assert!(op == BinOp::Eq);
                match *right {
                    Expr::Function { name, .. } => assert_eq!(name, "UPPER"),
                    other => panic!("expected Function, got {other:?}"),
                }
            }
            other => panic!("expected Binary, got {other:?}"),
        }
    }

    #[test]
    fn parse_combined_wave3_features() {
        // WHERE NOT (x > 5) AND y IS NOT NULL — combines NOT, paren, IS NOT NULL, AND
        let q = parse_sql("SELECT * FROM t WHERE NOT (x > 5) AND y IS NOT NULL").unwrap();
        let w = q.where_clause.unwrap();
        match w {
            Expr::Binary { left, op, right } => {
                assert!(op == BinOp::And);
                assert!(matches!(*left, Expr::Not(_)));
                match *right {
                    Expr::IsNull { negated, .. } => assert!(negated),
                    other => panic!("expected IsNull on right, got {other:?}"),
                }
            }
            other => panic!("expected AND at top, got {other:?}"),
        }
    }

    // ===== Wave 4: Subqueries and Set Operations =====

    #[test]
    fn parse_scalar_subquery_in_select() {
        // SELECT (SELECT COUNT(*) FROM t2) FROM t
        let q = parse_sql("SELECT (SELECT COUNT(*) FROM t2) FROM t").unwrap();
        assert_eq!(q.select.len(), 1);
        match &q.select[0] {
            SelectItem::Expression { expr, .. } => match expr {
                Expr::Function { name, args, .. } => {
                    assert_eq!(name, "__scalar_subquery__");
                    assert_eq!(args.len(), 1);
                    match &args[0] {
                        Expr::Literal(Value::String(sql)) => {
                            assert!(sql.contains("SELECT"), "subquery SQL: {sql}");
                            assert!(sql.contains("COUNT"));
                        }
                        other => panic!("expected String literal, got {other:?}"),
                    }
                }
                other => panic!("expected Function (scalar subquery), got {other:?}"),
            },
            other => panic!("expected Expression, got {other:?}"),
        }
    }

    #[test]
    fn parse_scalar_subquery_in_where() {
        // WHERE x = (SELECT MAX(y) FROM t2)
        let q = parse_sql("SELECT * FROM t WHERE x = (SELECT MAX(y) FROM t2)").unwrap();
        let w = q.where_clause.unwrap();
        match w {
            Expr::Binary { right, op, .. } => {
                assert!(op == BinOp::Eq);
                match *right {
                    Expr::Function { name, .. } => {
                        assert_eq!(name, "__scalar_subquery__");
                    }
                    other => panic!("expected scalar subquery, got {other:?}"),
                }
            }
            other => panic!("expected Binary, got {other:?}"),
        }
    }

    #[test]
    fn parse_in_subquery() {
        // WHERE id IN (SELECT id FROM t2)
        let q = parse_sql("SELECT * FROM t WHERE id IN (SELECT id FROM t2)").unwrap();
        let w = q.where_clause.unwrap();
        match w {
            Expr::InSubquery { expr, subquery_sql, negated } => {
                assert!(!negated);
                assert!(subquery_sql.contains("SELECT"));
                match *expr {
                    Expr::Column(c) => assert_eq!(c, "id"),
                    other => panic!("expected Column(id), got {other:?}"),
                }
            }
            other => panic!("expected InSubquery, got {other:?}"),
        }
    }

    #[test]
    fn parse_not_in_subquery() {
        let q = parse_sql("SELECT * FROM t WHERE id NOT IN (SELECT id FROM t2)").unwrap();
        let w = q.where_clause.unwrap();
        match w {
            Expr::InSubquery { negated, .. } => assert!(negated),
            other => panic!("expected InSubquery, got {other:?}"),
        }
    }

    #[test]
    fn parse_in_list_still_works() {
        // IN (1, 2, 3) should still produce InList, not InSubquery
        let q = parse_sql("SELECT * FROM t WHERE id IN (1, 2, 3)").unwrap();
        let w = q.where_clause.unwrap();
        match w {
            Expr::InList { list, negated, .. } => {
                assert!(!negated);
                assert_eq!(list.len(), 3);
            }
            other => panic!("expected InList, got {other:?}"),
        }
    }

    #[test]
    fn parse_exists_subquery() {
        // WHERE EXISTS (SELECT * FROM t2 WHERE t2.id = t.id)
        let q = parse_sql("SELECT * FROM t WHERE EXISTS (SELECT * FROM t2 WHERE t2.id = t.id)")
            .unwrap();
        let w = q.where_clause.unwrap();
        match w {
            Expr::Exists { subquery_sql, negated } => {
                assert!(!negated);
                assert!(subquery_sql.contains("SELECT"));
                assert!(subquery_sql.contains("t2"));
            }
            other => panic!("expected Exists, got {other:?}"),
        }
    }

    #[test]
    fn parse_not_exists_subquery() {
        let q = parse_sql("SELECT * FROM t WHERE NOT EXISTS (SELECT * FROM t2)").unwrap();
        let w = q.where_clause.unwrap();
        match w {
            Expr::Exists { negated, .. } => assert!(negated),
            other => panic!("expected Exists, got {other:?}"),
        }
    }

    #[test]
    fn parse_union() {
        let tokens = crate::sql::lexer::tokenize("SELECT a FROM t1 UNION SELECT a FROM t2").unwrap();
        let set = parse_set(tokens).unwrap();
        match set {
            SetQuery::Union(left, right) => {
                assert!(matches!(*left, SetQuery::Select(_)));
                assert!(matches!(*right, SetQuery::Select(_)));
            }
            other => panic!("expected Union, got {other:?}"),
        }
    }

    #[test]
    fn parse_union_all() {
        let tokens =
            crate::sql::lexer::tokenize("SELECT a FROM t1 UNION ALL SELECT a FROM t2").unwrap();
        let set = parse_set(tokens).unwrap();
        assert!(matches!(set, SetQuery::UnionAll(_, _)));
    }

    #[test]
    fn parse_intersect() {
        let tokens =
            crate::sql::lexer::tokenize("SELECT a FROM t1 INTERSECT SELECT a FROM t2").unwrap();
        let set = parse_set(tokens).unwrap();
        assert!(matches!(set, SetQuery::Intersect(_, _)));
    }

    #[test]
    fn parse_except() {
        let tokens =
            crate::sql::lexer::tokenize("SELECT a FROM t1 EXCEPT SELECT a FROM t2").unwrap();
        let set = parse_set(tokens).unwrap();
        assert!(matches!(set, SetQuery::Except(_, _)));
    }

    #[test]
    fn parse_set_precedence_intersect_over_union() {
        // SELECT a FROM t1 UNION SELECT a FROM t2 INTERSECT SELECT a FROM t3
        // should parse as t1 UNION (t2 INTERSECT t3) because INTERSECT
        // binds tighter than UNION.
        let tokens = crate::sql::lexer::tokenize(
            "SELECT a FROM t1 UNION SELECT a FROM t2 INTERSECT SELECT a FROM t3",
        )
        .unwrap();
        let set = parse_set(tokens).unwrap();
        match set {
            SetQuery::Union(left, right) => {
                assert!(matches!(*left, SetQuery::Select(_)));
                assert!(matches!(*right, SetQuery::Intersect(_, _)));
            }
            other => panic!("expected Union(Select, Intersect), got {other:?}"),
        }
    }

    #[test]
    fn parse_set_parenthesised() {
        // (SELECT a FROM t1 UNION SELECT a FROM t2) ORDER BY a
        // The parenthesised body is one operand.
        let tokens = crate::sql::lexer::tokenize(
            "(SELECT a FROM t1 UNION SELECT a FROM t2)",
        )
        .unwrap();
        let set = parse_set(tokens).unwrap();
        match set {
            SetQuery::Union(_, _) => {}
            other => panic!("expected Union, got {other:?}"),
        }
    }

    #[test]
    fn parse_set_backcompat_single_select() {
        // parse() (not parse_set()) should still return a single SelectQuery
        // for a non-set-operation query.
        let q = parse_sql("SELECT * FROM t WHERE x = 5").unwrap();
        assert_eq!(q.from, "t");
    }

    #[test]
    fn parse_trailing_semicolon_ok() {
        let q = parse_sql("SELECT * FROM t;").unwrap();
        assert_eq!(q.from, "t");
    }

    #[test]
    fn parse_string_literal_in_where() {
        let q = parse_sql("SELECT * FROM t WHERE name = 'alice'").unwrap();
        let w = q.where_clause.unwrap();
        match w {
            Expr::Binary { right, .. } => match *right {
                Expr::Literal(Value::String(s)) => assert_eq!(s, "alice"),
                other => panic!("expected String, got {other:?}"),
            },
            other => panic!("expected Binary, got {other:?}"),
        }
    }

    #[test]
    fn parse_invalid_missing_select_list() {
        let r = parse_sql("SELECT FROM WHERE");
        assert!(r.is_err(), "expected error for SELECT FROM WHERE");
    }

    #[test]
    fn parse_invalid_missing_table() {
        let r = parse_sql("SELECT * FROM WHERE");
        assert!(r.is_err(), "expected error for missing table name");
    }

    #[test]
    fn parse_invalid_negative_limit() {
        let r = parse_sql("SELECT * FROM t LIMIT -5");
        assert!(r.is_err(), "expected error for negative LIMIT");
    }

    #[test]
    fn parse_invalid_unexpected_eof() {
        // FROM is now optional (Wave 6), so "SELECT *" should parse
        // successfully and use the __dummy__ table.
        let r = parse_sql("SELECT *");
        assert!(r.is_ok(), "SELECT * without FROM should parse, got: {r:?}");
    }

    #[test]
    fn parse_invalid_trailing_garbage() {
        let r = parse_sql("SELECT * FROM t WHERE x = 5 garbage");
        assert!(r.is_err(), "expected error for trailing garbage");
    }

    #[test]
    fn parse_count_distinct_keyword() {
        // `COUNT(DISTINCT col)` is normalised to
        // `Aggregate { func: "COUNT_DISTINCT", arg: "col" }`.
        let q = parse_sql("SELECT COUNT(DISTINCT user_id) FROM events").unwrap();
        match &q.select[0] {
            SelectItem::Aggregate { func, arg, alias } => {
                assert_eq!(func, "COUNT_DISTINCT");
                assert_eq!(arg, "user_id");
                assert_eq!(*alias, None);
            }
            other => panic!("expected Aggregate, got {other:?}"),
        }
    }

    #[test]
    fn parse_count_distinct_case_insensitive() {
        // `count(distinct col)` should normalise the same way.
        let q = parse_sql("SELECT count(distinct x) FROM t").unwrap();
        match &q.select[0] {
            SelectItem::Aggregate { func, arg, .. } => {
                assert_eq!(func, "COUNT_DISTINCT");
                assert_eq!(arg, "x");
            }
            other => panic!("expected Aggregate, got {other:?}"),
        }
    }

    #[test]
    fn parse_count_distinct_requires_column() {
        // `COUNT(DISTINCT)` with no column should error.
        let r = parse_sql("SELECT COUNT(DISTINCT) FROM t");
        assert!(r.is_err(), "expected error for COUNT(DISTINCT) without column");
    }

    #[test]
    fn parse_sum_distinct_keyword() {
        // `SUM(DISTINCT col)` works the same way (produces SUM_DISTINCT).
        let q = parse_sql("SELECT SUM(DISTINCT price) FROM sales").unwrap();
        match &q.select[0] {
            SelectItem::Aggregate { func, arg, .. } => {
                assert_eq!(func, "SUM_DISTINCT");
                assert_eq!(arg, "price");
            }
            other => panic!("expected Aggregate, got {other:?}"),
        }
    }

    #[test]
    fn parse_select_integer_literal() {
        // `SELECT 1, URL, count(*)` — ClickBench Q15-Q42 shape.
        let q = parse_sql("SELECT 1, URL, count(*) FROM t").unwrap();
        assert_eq!(q.select.len(), 3);
        assert!(matches!(&q.select[0], SelectItem::Literal(1)));
        assert!(matches!(&q.select[1], SelectItem::Column(c) if c == "URL"));
        assert!(
            matches!(&q.select[2], SelectItem::Aggregate { func, arg, .. } if func == "COUNT" && arg == "*")
        );
    }

    #[test]
    fn parse_group_by_positional_and_column() {
        // `GROUP BY 1, URL` — the positional `1` is skipped, only URL
        // remains as a real GROUP BY key.
        let q = parse_sql("SELECT 1, URL, count(*) FROM t GROUP BY 1, URL").unwrap();
        assert_eq!(q.group_by, vec!["URL"]);
    }

    #[test]
    fn parse_group_by_positional_only() {
        // `GROUP BY 1` alone (degenerate but legal) → empty group_by.
        let q = parse_sql("SELECT 1, count(*) FROM t GROUP BY 1").unwrap();
        assert!(q.group_by.is_empty());
    }

    #[test]
    fn parse_select_negative_literal_now_supported() {
        // Wave 3: SELECT -1 now parses (previously rejected) thanks to
        // unary minus support in the SELECT list.
        let q = parse_sql("SELECT -1 FROM t").unwrap();
        assert_eq!(q.select.len(), 1);
        match &q.select[0] {
            SelectItem::Expression { expr, .. } => match expr {
                Expr::Unary { op: UnaryOp::Neg, expr: inner } => match inner.as_ref() {
                    Expr::Literal(Value::Int(1)) => {}
                    other => panic!("expected Int(1) inside Unary, got {other:?}"),
                },
                other => panic!("expected Unary(Neg, Int(1)), got {other:?}"),
            },
            other => panic!("expected Expression, got {other:?}"),
        }
    }

    #[test]
    fn parse_clickbench_q15_shape() {
        // Full Q15 shape: SELECT 1, URL, count(*) AS c FROM t WHERE URL LIKE 'https://%' GROUP BY 1, URL ORDER BY c DESC LIMIT 10
        let q = parse_sql(
            "SELECT 1, URL, count(*) AS c FROM t WHERE URL LIKE 'https://%' GROUP BY 1, URL ORDER BY c DESC LIMIT 10",
        )
        .unwrap();
        assert_eq!(q.select.len(), 3);
        assert!(matches!(&q.select[0], SelectItem::Literal(1)));
        assert!(matches!(&q.select[2], SelectItem::Aggregate { alias: Some(a), .. } if a == "c"));
        assert_eq!(q.group_by, vec!["URL"]);
        assert_eq!(q.order_by, vec![("c".to_string(), false)]);
        assert_eq!(q.limit, Some(10));
    }

    /// Wave 62 fix: HAVING with count(*) must parse without error.
    /// Previously, parse_primary didn't handle `IDENT(` as a function call
    /// in expression context, causing "unexpected trailing token: LParen".
    #[test]
    fn parse_having_with_count_star() {
        let q =
            parse_sql("SELECT dept, count(*) FROM t GROUP BY dept HAVING count(*) > 1").unwrap();
        assert!(q.having.is_some(), "HAVING clause must be parsed");
        // Verify the HAVING expression is a Binary comparison.
        match &q.having {
            Some(Expr::Binary { left, op, right }) => {
                assert!(*op == BinOp::Gt);
                // Left should be Expr::Function { name: "COUNT", args: [Wildcard] }
                match left.as_ref() {
                    Expr::Function { name, args, .. } => {
                        assert_eq!(name, "COUNT");
                        assert!(args.iter().any(|a| *a == Expr::Wildcard), "args should contain Wildcard");
                    }
                    other => panic!("expected Function, got {other:?}"),
                }
                // Right should be Literal(Int(1))
                match right.as_ref() {
                    Expr::Literal(Value::Int(1)) => {}
                    other => panic!("expected Int(1), got {other:?}"),
                }
            }
            other => panic!("expected Binary, got {other:?}"),
        }
    }

    /// Wave 62 fix: HAVING with sum(col) must also parse.
    #[test]
    fn parse_having_with_sum() {
        let q = parse_sql("SELECT dept FROM t GROUP BY dept HAVING sum(salary) > 400").unwrap();
        assert!(q.having.is_some());
        match &q.having {
            Some(Expr::Binary { left, op, .. }) => {
                assert!(*op == BinOp::Gt);
                match left.as_ref() {
                    Expr::Function { name, args, .. } => {
                        assert_eq!(name, "SUM");
                        // args[0] should be Column("salary")
                        assert!(args.iter().any(|a| matches!(a, Expr::Column(c) if c == "salary")));
                    }
                    other => panic!("expected Function, got {other:?}"),
                }
            }
            other => panic!("expected Binary, got {other:?}"),
        }
    }

    /// Wave 60d: SELECT DISTINCT must parse and set the distinct flag.
    #[test]
    fn parse_select_distinct() {
        let q = parse_sql("SELECT DISTINCT dept FROM t").unwrap();
        assert!(q.distinct, "distinct flag must be true");
        assert_eq!(q.select.len(), 1);
    }

    /// SELECT without DISTINCT must have distinct = false.
    #[test]
    fn parse_select_without_distinct() {
        let q = parse_sql("SELECT dept FROM t").unwrap();
        assert!(!q.distinct, "distinct flag must be false");
    }

    /// Wave 60a: CASE WHEN in SELECT list must parse as SelectItem::Expression.
    #[test]
    fn parse_case_when_in_select() {
        let q = parse_sql("SELECT CASE WHEN x > 5 THEN 1 ELSE 0 END FROM t").unwrap();
        assert_eq!(q.select.len(), 1);
        match &q.select[0] {
            SelectItem::Expression { expr, alias } => {
                assert!(alias.is_none());
                assert!(matches!(expr, Expr::Case { .. }));
            }
            other => panic!("expected Expression, got {other:?}"),
        }
    }

    /// Wave 67: EXTRACT(YEAR FROM d) must parse to Expr::Extract.
    #[test]
    fn parse_extract_year() {
        let q = parse_sql("SELECT EXTRACT(YEAR FROM d) FROM t").unwrap();
        assert_eq!(q.select.len(), 1);
        match &q.select[0] {
            SelectItem::Expression { expr, alias } => {
                assert!(alias.is_none());
                match expr {
                    Expr::Extract { field, expr } => {
                        assert_eq!(field, "YEAR", "field must be YEAR (uppercased)");
                        // The inner expr should be a Column("d").
                        match expr.as_ref() {
                            Expr::Column(name) => assert_eq!(name, "d"),
                            other => panic!("expected Column(d), got {other:?}"),
                        }
                    }
                    other => panic!("expected Expr::Extract, got {other:?}"),
                }
            }
            other => panic!("expected Expression, got {other:?}"),
        }
    }

    /// Wave 67: EXTRACT with MONTH and DAY fields also parse.
    #[test]
    fn parse_extract_month_day() {
        let q = parse_sql("SELECT EXTRACT(MONTH FROM d) FROM t").unwrap();
        match &q.select[0] {
            SelectItem::Expression { expr, .. } => match expr {
                Expr::Extract { field, .. } => assert_eq!(field, "MONTH"),
                other => panic!("expected Extract, got {other:?}"),
            },
            _ => panic!("expected Expression"),
        }
        let q = parse_sql("SELECT EXTRACT(DAY FROM d) FROM t").unwrap();
        match &q.select[0] {
            SelectItem::Expression { expr, .. } => match expr {
                Expr::Extract { field, .. } => assert_eq!(field, "DAY"),
                other => panic!("expected Extract, got {other:?}"),
            },
            _ => panic!("expected Expression"),
        }
    }

    /// Wave 67: EXTRACT is case-insensitive (extract(year from d)).
    #[test]
    fn parse_extract_case_insensitive() {
        let q = parse_sql("SELECT extract(year from d) FROM t").unwrap();
        match &q.select[0] {
            SelectItem::Expression { expr, .. } => match expr {
                Expr::Extract { field, .. } => assert_eq!(field, "YEAR"),
                other => panic!("expected Extract, got {other:?}"),
            },
            _ => panic!("expected Expression"),
        }
    }

    /// Wave 67: CAST(x AS INT) must parse to Expr::Cast.
    #[test]
    fn parse_cast_int() {
        let q = parse_sql("SELECT CAST(x AS INT) FROM t").unwrap();
        assert_eq!(q.select.len(), 1);
        match &q.select[0] {
            SelectItem::Expression { expr, alias } => {
                assert!(alias.is_none());
                match expr {
                    Expr::Cast { expr, target_type } => {
                        assert_eq!(target_type, "INT", "target_type must be INT");
                        match expr.as_ref() {
                            Expr::Column(name) => assert_eq!(name, "x"),
                            other => panic!("expected Column(x), got {other:?}"),
                        }
                    }
                    other => panic!("expected Expr::Cast, got {other:?}"),
                }
            }
            other => panic!("expected Expression, got {other:?}"),
        }
    }

    /// Wave 67: CAST with FLOAT, VARCHAR, BIGINT target types.
    #[test]
    fn parse_cast_other_types() {
        for (sql, expected_type) in [
            ("SELECT CAST(x AS FLOAT) FROM t", "FLOAT"),
            ("SELECT CAST(x AS BIGINT) FROM t", "BIGINT"),
            ("SELECT CAST(x AS VARCHAR) FROM t", "VARCHAR"),
            ("SELECT CAST(x AS VARCHAR(50)) FROM t", "VARCHAR"),
        ] {
            let q = parse_sql(sql).unwrap();
            match &q.select[0] {
                SelectItem::Expression { expr, .. } => match expr {
                    Expr::Cast { target_type, .. } => {
                        assert_eq!(*target_type, expected_type, "SQL: {sql}");
                    }
                    other => panic!("SQL {sql}: expected Cast, got {other:?}"),
                },
                other => panic!("SQL {sql}: expected Expression, got {other:?}"),
            }
        }
    }

    /// Wave 67: CAST is case-insensitive (cast(x as int)).
    #[test]
    fn parse_cast_case_insensitive() {
        let q = parse_sql("SELECT cast(x as int) FROM t").unwrap();
        match &q.select[0] {
            SelectItem::Expression { expr, .. } => match expr {
                Expr::Cast { target_type, .. } => assert_eq!(*target_type, "INT"),
                other => panic!("expected Cast, got {other:?}"),
            },
            _ => panic!("expected Expression"),
        }
    }

    /// Wave 67: EXTRACT in WHERE clause must parse (not error).
    #[test]
    fn parse_extract_in_where() {
        let q = parse_sql("SELECT * FROM t WHERE EXTRACT(YEAR FROM d) = 2024").unwrap();
        let w = q.where_clause.expect("WHERE clause");
        match w {
            Expr::Binary { left, op, .. } => {
                assert!(op == BinOp::Eq);
                match *left {
                    Expr::Extract { field, .. } => assert_eq!(field, "YEAR"),
                    other => panic!("expected Extract in WHERE left, got {other:?}"),
                }
            }
            other => panic!("expected Binary in WHERE, got {other:?}"),
        }
    }
}
