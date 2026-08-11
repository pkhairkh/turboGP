//! # DML parser: INSERT / UPDATE / DELETE.
//!
//! Supports:
//! - `INSERT INTO table (col1, col2, ...) VALUES (v1, v2, ...), (v3, v4, ...)`
//! - `INSERT INTO table VALUES (v1, v2, ...)`
//! - `INSERT INTO table (cols) SELECT ...` (Wave 5)
//! - `INSERT INTO table ... ON CONFLICT (cols) DO UPDATE SET ...` (Wave 5)
//! - `INSERT INTO table ... ON CONFLICT DO NOTHING` (Wave 5)
//! - `INSERT INTO table ... RETURNING ...` (Wave 5)
//! - `UPDATE table SET col = expr, ... WHERE condition RETURNING ...` (Wave 5)
//! - `DELETE FROM table WHERE condition RETURNING ...` (Wave 5)
//!
//! Since Wave 5, SET expressions and WHERE predicates are stored as parsed
//! [`crate::sql::ast::Expr`] rather than raw token strings. This enables
//! the executor to evaluate predicates without re-tokenising them.

use crate::sql::ast::Expr;
use crate::sql::lexer::{tokenize, Token};
use crate::sql::parser::{parse_expression, parse_set, SelectItem, SetQuery};

/// A parsed INSERT statement.
#[derive(Debug, Clone)]
pub struct Insert {
    /// Table name (may be schema.table).
    pub table: String,
    /// Optional column list. If None, all columns in table order.
    pub columns: Option<Vec<String>>,
    /// Values: one Vec per row, one String per column. Populated when
    /// the INSERT uses `VALUES (...)`. Empty when `source` is
    /// [`InsertSource::Select`].
    pub values: Vec<Vec<String>>,
    /// The data source for the INSERT. `Values` reuses the `values` field
    /// for backward compatibility; `Select` stores a parsed [`SetQuery`]
    /// for `INSERT INTO ... SELECT ...`.
    pub source: InsertSource,
    /// Optional RETURNING clause (list of columns or `*`).
    pub returning: Option<Vec<SelectItem>>,
    /// Optional ON CONFLICT clause (UPSERT).
    pub on_conflict: Option<OnConflict>,
}

/// The data source of an INSERT statement.
#[derive(Debug, Clone)]
pub enum InsertSource {
    /// `VALUES (v1, ...), (v2, ...)` — row literals. The actual values
    /// are stored in [`Insert::values`] for backward compatibility with
    /// existing consumers.
    Values,
    /// `SELECT ...` — a subquery providing the rows to insert.
    Select(SetQuery),
}

/// ON CONFLICT clause (UPSERT).
#[derive(Debug, Clone)]
pub enum OnConflict {
    /// `ON CONFLICT DO NOTHING` — silently skip conflicting rows.
    DoNothing,
    /// `ON CONFLICT (cols) DO UPDATE SET col = expr, ...` — update
    /// existing rows with new values. The `excluded` pseudo-table
    /// refers to the proposed insertion row.
    DoUpdate {
        /// Columns targeted by the conflict (e.g. the PRIMARY KEY columns).
        target: Vec<String>,
        /// `(column_name, expression)` assignments. The expression can
        /// reference `excluded.col` to use the proposed value.
        assignments: Vec<(String, Expr)>,
        /// Optional WHERE clause restricting which conflicts get updated.
        where_clause: Option<Expr>,
    },
}

/// A parsed UPDATE statement.
#[derive(Debug, Clone)]
pub struct Update {
    /// Table name.
    pub table: String,
    /// `(column_name, expression)` pairs. The expression is a parsed
    /// [`Expr`] since Wave 5 (was `String` before).
    pub assignments: Vec<(String, Expr)>,
    /// Optional WHERE clause as a parsed [`Expr`] (was `String` before).
    pub where_clause: Option<Expr>,
    /// Optional RETURNING clause.
    pub returning: Option<Vec<SelectItem>>,
}

/// A parsed DELETE statement.
#[derive(Debug, Clone)]
pub struct Delete {
    /// Table name.
    pub table: String,
    /// Optional WHERE clause as a parsed [`Expr`] (was `String` before).
    pub where_clause: Option<Expr>,
    /// Optional RETURNING clause.
    pub returning: Option<Vec<SelectItem>>,
}

/// The result of parsing a DML statement.
#[derive(Debug, Clone)]
pub enum DmlStatement {
    /// INSERT statement.
    Insert(Insert),
    /// UPDATE statement.
    Update(Update),
    /// DELETE statement.
    Delete(Delete),
}

/// Parse a DML string. Returns `None` if not a DML statement.
///
/// # Errors
///
/// Returns `Err(String)` for malformed DML (missing keywords, unterminated
/// value lists, unparseable expressions, etc.).
pub fn parse_dml(sql: &str) -> Result<Option<DmlStatement>, String> {
    let tokens = tokenize(sql)?;
    if tokens.is_empty() {
        return Ok(None);
    }
    let first = match &tokens[0] {
        Token::Keyword(k) => k.as_str(),
        _ => return Ok(None),
    };
    match first {
        "INSERT" => parse_insert(&tokens[1..]).map(|s| Some(DmlStatement::Insert(s))),
        "UPDATE" => parse_update(&tokens[1..]).map(|s| Some(DmlStatement::Update(s))),
        "DELETE" => parse_delete(&tokens[1..]).map(|s| Some(DmlStatement::Delete(s))),
        _ => Ok(None),
    }
}

fn parse_insert(tokens: &[Token]) -> Result<Insert, String> {
    let mut pos = 0;
    // Expect INTO
    if pos >= tokens.len() {
        return Err("expected INTO after INSERT".into());
    }
    match &tokens[pos] {
        Token::Keyword(k) if k == "INTO" => pos += 1,
        other => return Err(format!("expected INTO, got {other:?}")),
    }
    // Table name
    let table = parse_qualified_name(tokens, &mut pos)?;
    // Optional column list
    let columns = if pos < tokens.len() {
        if let Token::LParen = &tokens[pos] {
            // Save position — this LParen could be either a column list
            // (followed by VALUES/SELECT) or part of a VALUES row if no
            // column list is present. We assume column list here.
            pos += 1;
            let mut cols = Vec::new();
            loop {
                if pos >= tokens.len() {
                    return Err("unterminated column list".into());
                }
                match &tokens[pos] {
                    Token::Ident(s) => cols.push(s.clone()),
                    Token::RParen => {
                        pos += 1;
                        break;
                    }
                    other => return Err(format!("expected column or ), got {other:?}")),
                }
                pos += 1;
                if pos >= tokens.len() {
                    return Err("unterminated column list".into());
                }
                match &tokens[pos] {
                    Token::Comma => pos += 1,
                    Token::RParen => {
                        pos += 1;
                        break;
                    }
                    _ => {}
                }
            }
            Some(cols)
        } else {
            None
        }
    } else {
        None
    };

    // Now expect either VALUES, SELECT, or ON CONFLICT (with no source).
    let (values, source) = if pos < tokens.len() {
        match &tokens[pos] {
            Token::Keyword(k) if k == "VALUES" => {
                pos += 1;
                let vs = parse_values_rows(tokens, &mut pos)?;
                (vs, InsertSource::Values)
            }
            Token::Keyword(k) if k == "SELECT" => {
                // INSERT INTO ... SELECT ... — parse the SELECT as a SetQuery.
                let select_tokens: Vec<Token> = tokens[pos..]
                    .iter()
                    .take_while(|t| !matches!(t, Token::Keyword(k) if k == "ON" || k == "RETURNING"))
                    .cloned()
                    .collect();
                let consumed = select_tokens.len();
                let mut select_with_eof = select_tokens;
                select_with_eof.push(Token::EOF);
                let set = parse_set(select_with_eof)?;
                pos += consumed;
                (Vec::new(), InsertSource::Select(set))
            }
            _ => (Vec::new(), InsertSource::Values),
        }
    } else {
        (Vec::new(), InsertSource::Values)
    };

    // Optional ON CONFLICT clause.
    let on_conflict = if pos < tokens.len() {
        if let Token::Keyword(k) = &tokens[pos] {
            if k == "ON" {
                pos += 1;
                if pos >= tokens.len() || !matches!(&tokens[pos], Token::Keyword(k) if k == "CONFLICT") {
                    return Err("expected CONFLICT after ON".into());
                }
                pos += 1;
                Some(parse_on_conflict(tokens, &mut pos)?)
            } else {
                None
            }
        } else {
            None
        }
    } else {
        None
    };

    // Optional RETURNING clause.
    let returning = parse_returning(tokens, &mut pos)?;

    Ok(Insert {
        table,
        columns,
        values,
        source,
        returning,
        on_conflict,
    })
}

/// Parse `VALUES (v1, v2, ...), (v3, v4, ...)` rows.
fn parse_values_rows(tokens: &[Token], pos: &mut usize) -> Result<Vec<Vec<String>>, String> {
    let mut values = Vec::new();
    loop {
        if *pos >= tokens.len() {
            return Err("expected ( after VALUES".into());
        }
        match &tokens[*pos] {
            Token::LParen => *pos += 1,
            Token::Semicolon | Token::EOF => break,
            other => return Err(format!("expected ( or end, got {other:?}")),
        }
        let mut row = Vec::new();
        loop {
            if *pos >= tokens.len() {
                return Err("unterminated value list".into());
            }
            match &tokens[*pos] {
                Token::RParen => {
                    *pos += 1;
                    break;
                }
                Token::Comma => *pos += 1,
                _ => {
                    let v = token_to_value_string(tokens, pos)?;
                    row.push(v);
                }
            }
        }
        values.push(row);
        // Check for another row or end
        if *pos >= tokens.len() {
            break;
        }
        match &tokens[*pos] {
            Token::Comma => *pos += 1,
            Token::Semicolon | Token::EOF => break,
            _ => break,
        }
    }
    Ok(values)
}

/// Parse the body of an ON CONFLICT clause (after `ON CONFLICT`).
fn parse_on_conflict(tokens: &[Token], pos: &mut usize) -> Result<OnConflict, String> {
    // Optional conflict target: (col1, col2, ...)
    let target = if *pos < tokens.len() {
        if let Token::LParen = &tokens[*pos] {
            *pos += 1;
            let mut cols = Vec::new();
            loop {
                if *pos >= tokens.len() {
                    return Err("unterminated conflict target".into());
                }
                match &tokens[*pos] {
                    Token::Ident(s) => cols.push(s.clone()),
                    Token::RParen => {
                        *pos += 1;
                        break;
                    }
                    other => return Err(format!("expected column or ) in conflict target, got {other:?}")),
                }
                *pos += 1;
                if *pos >= tokens.len() {
                    return Err("unterminated conflict target".into());
                }
                match &tokens[*pos] {
                    Token::Comma => *pos += 1,
                    Token::RParen => {
                        *pos += 1;
                        break;
                    }
                    _ => {}
                }
            }
            cols
        } else {
            Vec::new()
        }
    } else {
        Vec::new()
    };

    // Expect DO
    if *pos >= tokens.len() || !matches!(&tokens[*pos], Token::Keyword(k) if k == "DO") {
        return Err("expected DO after ON CONFLICT".into());
    }
    *pos += 1;

    // DO NOTHING or DO UPDATE
    if *pos >= tokens.len() {
        return Err("expected NOTHING or UPDATE after DO".into());
    }
    match &tokens[*pos] {
        Token::Keyword(k) if k == "NOTHING" => {
            *pos += 1;
            Ok(OnConflict::DoNothing)
        }
        Token::Keyword(k) if k == "UPDATE" => {
            *pos += 1;
            // Expect SET
            if *pos >= tokens.len() || !matches!(&tokens[*pos], Token::Keyword(k) if k == "SET") {
                return Err("expected SET after DO UPDATE".into());
            }
            *pos += 1;
            let assignments = parse_set_assignments(tokens, pos)?;
            // Optional WHERE
            let where_clause = if *pos < tokens.len() {
                if let Token::Keyword(k) = &tokens[*pos] {
                    if k == "WHERE" {
                        *pos += 1;
                        Some(parse_expr_from_tokens(tokens, pos)?)
                    } else {
                        None
                    }
                } else {
                    None
                }
            } else {
                None
            };
            Ok(OnConflict::DoUpdate { target, assignments, where_clause })
        }
        other => Err(format!("expected NOTHING or UPDATE after DO, got {other:?}")),
    }
}

/// Parse a RETURNING clause: `RETURNING col1, col2, *` (if present).
fn parse_returning(tokens: &[Token], pos: &mut usize) -> Result<Option<Vec<SelectItem>>, String> {
    if *pos >= tokens.len() {
        return Ok(None);
    }
    if !matches!(&tokens[*pos], Token::Keyword(k) if k == "RETURNING") {
        return Ok(None);
    }
    *pos += 1;
    let mut items = Vec::new();
    loop {
        if *pos >= tokens.len() {
            return Err("unterminated RETURNING clause".into());
        }
        match &tokens[*pos] {
            Token::Op(op) if op == "*" => {
                items.push(SelectItem::Star);
                *pos += 1;
            }
            Token::Ident(s) => {
                items.push(SelectItem::Column(s.clone()));
                *pos += 1;
            }
            other => return Err(format!("expected column or * in RETURNING, got {other:?}")),
        }
        if *pos >= tokens.len() {
            break;
        }
        match &tokens[*pos] {
            Token::Comma => *pos += 1,
            _ => break,
        }
    }
    Ok(Some(items))
}

fn parse_update(tokens: &[Token]) -> Result<Update, String> {
    let mut pos = 0;
    let table = parse_qualified_name(tokens, &mut pos)?;
    // Expect SET
    if pos >= tokens.len() {
        return Err("expected SET after table name".into());
    }
    match &tokens[pos] {
        Token::Keyword(k) if k == "SET" => pos += 1,
        other => return Err(format!("expected SET, got {other:?}")),
    }
    let assignments = parse_set_assignments(tokens, &mut pos)?;
    // Optional WHERE
    let where_clause = if pos < tokens.len() {
        match &tokens[pos] {
            Token::Keyword(k) if k == "WHERE" => {
                pos += 1;
                Some(parse_expr_from_tokens(tokens, &mut pos)?)
            }
            _ => None,
        }
    } else {
        None
    };
    // Optional RETURNING
    let returning = parse_returning(tokens, &mut pos)?;
    Ok(Update {
        table,
        assignments,
        where_clause,
        returning,
    })
}

fn parse_delete(tokens: &[Token]) -> Result<Delete, String> {
    let mut pos = 0;
    // Expect FROM
    if pos >= tokens.len() {
        return Err("expected FROM after DELETE".into());
    }
    match &tokens[pos] {
        Token::Keyword(k) if k == "FROM" => pos += 1,
        other => return Err(format!("expected FROM, got {other:?}")),
    }
    let table = parse_qualified_name(tokens, &mut pos)?;
    // Optional WHERE
    let where_clause = if pos < tokens.len() {
        match &tokens[pos] {
            Token::Keyword(k) if k == "WHERE" => {
                pos += 1;
                Some(parse_expr_from_tokens(tokens, &mut pos)?)
            }
            _ => None,
        }
    } else {
        None
    };
    // Optional RETURNING
    let returning = parse_returning(tokens, &mut pos)?;
    Ok(Delete {
        table,
        where_clause,
        returning,
    })
}

/// Parse `col = expr, col = expr, ...` assignments. Returns the parsed
/// `(column_name, Expr)` pairs. The expression is parsed using the
/// main parser's expression grammar.
fn parse_set_assignments(
    tokens: &[Token],
    pos: &mut usize,
) -> Result<Vec<(String, Expr)>, String> {
    let mut assignments = Vec::new();
    loop {
        if *pos >= tokens.len() {
            return Err("expected column name in SET".into());
        }
        let col = match &tokens[*pos] {
            Token::Ident(s) => s.clone(),
            other => return Err(format!("expected column name in SET, got {other:?}")),
        };
        *pos += 1;
        if *pos >= tokens.len() {
            return Err("expected = after column name".into());
        }
        match &tokens[*pos] {
            Token::Op(op) if op == "=" => *pos += 1,
            other => return Err(format!("expected = after column name, got {other:?}")),
        }
        let expr = parse_expr_from_tokens(tokens, pos)?;
        assignments.push((col, expr));
        // Check for another assignment
        if *pos < tokens.len() {
            match &tokens[*pos] {
                Token::Comma => {
                    *pos += 1;
                    continue;
                }
                _ => {}
            }
        }
        break;
    }
    Ok(assignments)
}

/// Parse an [`Expr`] from a slice of tokens starting at `pos`. Stops at
/// keywords that terminate an expression context (WHERE, SET, RETURNING,
/// ON, etc.) or at a comma/semicolon/EOF. Advances `pos` past the parsed
/// tokens.
fn parse_expr_from_tokens(tokens: &[Token], pos: &mut usize) -> Result<Expr, String> {
    // Find the end of the expression by scanning for terminator tokens.
    let start = *pos;
    let mut depth: i32 = 0;
    let mut end = start;
    while end < tokens.len() {
        match &tokens[end] {
            Token::LParen => depth += 1,
            Token::RParen => {
                if depth == 0 {
                    break;
                }
                depth -= 1;
            }
            Token::Comma | Token::Semicolon | Token::EOF if depth == 0 => break,
            Token::Keyword(k) if depth == 0 => {
                // Stop at clause boundaries.
                if matches!(
                    k.as_str(),
                    "WHERE" | "SET" | "RETURNING" | "ON" | "VALUES" | "SELECT" | "FROM" | "GROUP"
                        | "ORDER" | "HAVING" | "LIMIT" | "CONFLICT" | "DO" | "NOTHING" | "UPDATE"
                ) {
                    break;
                }
            }
            _ => {}
        }
        end += 1;
    }
    // Build a token slice with EOF and delegate to the main parser's
    // expression grammar via the public parse_expression entry point.
    let mut sub_tokens: Vec<Token> = tokens[start..end].to_vec();
    sub_tokens.push(Token::EOF);
    *pos = end;

    parse_expression(sub_tokens)
}

fn parse_qualified_name(tokens: &[Token], pos: &mut usize) -> Result<String, String> {
    if *pos >= tokens.len() {
        return Err("expected table name".into());
    }
    let first = match &tokens[*pos] {
        Token::Ident(s) => s.clone(),
        Token::Keyword(k) => k.clone(),
        other => return Err(format!("expected table name, got {other:?}")),
    };
    *pos += 1;
    if *pos < tokens.len() {
        if let Token::Op(op) = &tokens[*pos] {
            if op == "." {
                *pos += 1;
                if *pos >= tokens.len() {
                    return Err("expected name after .".into());
                }
                let second = match &tokens[*pos] {
                    Token::Ident(s) => s.clone(),
                    Token::Keyword(k) => k.clone(),
                    other => return Err(format!("expected name after ., got {other:?}")),
                };
                *pos += 1;
                return Ok(format!("{first}.{second}"));
            }
        }
    }
    Ok(first)
}

fn token_to_value_string(tokens: &[Token], pos: &mut usize) -> Result<String, String> {
    if *pos >= tokens.len() {
        return Err("expected value".into());
    }
    let s = match &tokens[*pos] {
        Token::Int(n) => n.to_string(),
        Token::Float(f) => f.to_string(),
        Token::String(s) => format!("'{s}'"),
        Token::Hex(b) => {
            format!("x'{}'", b.iter().map(|b| format!("{:02x}", b)).collect::<String>())
        }
        Token::Keyword(k) if k == "NULL" => "NULL".into(),
        Token::Keyword(k) => k.clone(),
        Token::Ident(s) => s.clone(),
        Token::Op(op) => op.clone(),
        other => return Err(format!("unsupported value: {other:?}")),
    };
    *pos += 1;
    Ok(s)
}

// -----------------------------------------------------------------------
// Tests
// -----------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_insert_with_columns() {
        let sql = "INSERT INTO users (id, name) VALUES (1, 'Alice'), (2, 'Bob')";
        let dml = parse_dml(sql).unwrap().unwrap();
        match dml {
            DmlStatement::Insert(ins) => {
                assert_eq!(ins.table, "users");
                assert_eq!(ins.columns, Some(vec!["id".into(), "name".into()]));
                assert_eq!(ins.values.len(), 2);
                assert_eq!(ins.values[0], vec!["1", "'Alice'"]);
                assert_eq!(ins.values[1], vec!["2", "'Bob'"]);
                assert!(matches!(ins.source, InsertSource::Values));
                assert!(ins.returning.is_none());
                assert!(ins.on_conflict.is_none());
            }
            _ => panic!("expected Insert"),
        }
    }

    #[test]
    fn parse_insert_without_columns() {
        let sql = "INSERT INTO t VALUES (1, 2, 3)";
        let dml = parse_dml(sql).unwrap().unwrap();
        match dml {
            DmlStatement::Insert(ins) => {
                assert_eq!(ins.table, "t");
                assert_eq!(ins.columns, None);
                assert_eq!(ins.values, vec![vec!["1", "2", "3"]]);
            }
            _ => panic!("expected Insert"),
        }
    }

    #[test]
    fn parse_insert_qualified() {
        let sql = "INSERT INTO HR.Employees (id) VALUES (1)";
        let dml = parse_dml(sql).unwrap().unwrap();
        match dml {
            DmlStatement::Insert(ins) => assert_eq!(ins.table, "HR.Employees"),
            _ => panic!("expected Insert"),
        }
    }

    #[test]
    fn parse_insert_select() {
        let sql = "INSERT INTO t (a, b) SELECT x, y FROM t2";
        let dml = parse_dml(sql).unwrap().unwrap();
        match dml {
            DmlStatement::Insert(ins) => {
                assert_eq!(ins.table, "t");
                assert_eq!(ins.columns, Some(vec!["a".into(), "b".into()]));
                assert!(ins.values.is_empty());
                match ins.source {
                    InsertSource::Select(_) => {}
                    other => panic!("expected Select source, got {other:?}"),
                }
            }
            _ => panic!("expected Insert"),
        }
    }

    #[test]
    fn parse_insert_select_no_columns() {
        let sql = "INSERT INTO t SELECT * FROM t2";
        let dml = parse_dml(sql).unwrap().unwrap();
        match dml {
            DmlStatement::Insert(ins) => {
                assert_eq!(ins.table, "t");
                match ins.source {
                    InsertSource::Select(_) => {}
                    other => panic!("expected Select source, got {other:?}"),
                }
            }
            _ => panic!("expected Insert"),
        }
    }

    #[test]
    fn parse_insert_returning() {
        let sql = "INSERT INTO t VALUES (1) RETURNING id";
        let dml = parse_dml(sql).unwrap().unwrap();
        match dml {
            DmlStatement::Insert(ins) => {
                assert!(ins.returning.is_some());
                let returning = ins.returning.unwrap();
                assert_eq!(returning.len(), 1);
                assert!(matches!(&returning[0], SelectItem::Column(c) if c == "id"));
            }
            _ => panic!("expected Insert"),
        }
    }

    #[test]
    fn parse_insert_returning_star() {
        let sql = "INSERT INTO t VALUES (1) RETURNING *";
        let dml = parse_dml(sql).unwrap().unwrap();
        match dml {
            DmlStatement::Insert(ins) => {
                let returning = ins.returning.unwrap();
                assert_eq!(returning.len(), 1);
                assert!(matches!(&returning[0], SelectItem::Star));
            }
            _ => panic!("expected Insert"),
        }
    }

    #[test]
    fn parse_insert_on_conflict_do_nothing() {
        let sql = "INSERT INTO t VALUES (1) ON CONFLICT DO NOTHING";
        let dml = parse_dml(sql).unwrap().unwrap();
        match dml {
            DmlStatement::Insert(ins) => {
                assert!(matches!(ins.on_conflict, Some(OnConflict::DoNothing)));
            }
            _ => panic!("expected Insert"),
        }
    }

    #[test]
    fn parse_insert_on_conflict_do_update() {
        let sql =
            "INSERT INTO t (id, x) VALUES (1, 2) ON CONFLICT (id) DO UPDATE SET x = excluded.x";
        let dml = parse_dml(sql).unwrap().unwrap();
        match dml {
            DmlStatement::Insert(ins) => match ins.on_conflict {
                Some(OnConflict::DoUpdate { target, assignments, where_clause }) => {
                    assert_eq!(target, vec!["id".to_string()]);
                    assert_eq!(assignments.len(), 1);
                    assert_eq!(assignments[0].0, "x");
                    assert!(where_clause.is_none());
                }
                other => panic!("expected DoUpdate, got {other:?}"),
            },
            _ => panic!("expected Insert"),
        }
    }

    #[test]
    fn parse_update_with_ast_expr() {
        // Wave 5: SET expression is now an ast::Expr, not a string.
        let sql = "UPDATE users SET name = 'Bob', active = 1 WHERE id = 5";
        let dml = parse_dml(sql).unwrap().unwrap();
        match dml {
            DmlStatement::Update(up) => {
                assert_eq!(up.table, "users");
                assert_eq!(up.assignments.len(), 2);
                assert_eq!(up.assignments[0].0, "name");
                match &up.assignments[0].1 {
                    Expr::Literal(crate::sql::ast::Value::String(s)) => assert_eq!(s, "Bob"),
                    other => panic!("expected String literal 'Bob', got {other:?}"),
                }
                assert_eq!(up.assignments[1].0, "active");
                match &up.assignments[1].1 {
                    Expr::Literal(crate::sql::ast::Value::Int(1)) => {}
                    other => panic!("expected Int(1), got {other:?}"),
                }
                assert!(up.where_clause.is_some());
                match up.where_clause {
                    Some(Expr::Binary { op, .. }) => {
                        use crate::sql::ast::BinOp;
                        assert!(matches!(op, BinOp::Eq));
                    }
                    other => panic!("expected Binary in WHERE, got {other:?}"),
                }
            }
            _ => panic!("expected Update"),
        }
    }

    #[test]
    fn parse_update_complex_expr() {
        // Wave 5: SET col = col + 1 — expression with arithmetic.
        let sql = "UPDATE t SET count = count + 1 WHERE id = 5";
        let dml = parse_dml(sql).unwrap().unwrap();
        match dml {
            DmlStatement::Update(up) => {
                assert_eq!(up.assignments[0].0, "count");
                match &up.assignments[0].1 {
                    Expr::Binary { op, .. } => {
                        use crate::sql::ast::BinOp;
                        assert!(*op == BinOp::Add);
                    }
                    other => panic!("expected Binary(Add), got {other:?}"),
                }
            }
            _ => panic!("expected Update"),
        }
    }

    #[test]
    fn parse_update_returning() {
        let sql = "UPDATE t SET x = 1 WHERE id = 5 RETURNING *";
        let dml = parse_dml(sql).unwrap().unwrap();
        match dml {
            DmlStatement::Update(up) => {
                let returning = up.returning.expect("RETURNING clause");
                assert_eq!(returning.len(), 1);
                assert!(matches!(&returning[0], SelectItem::Star));
            }
            _ => panic!("expected Update"),
        }
    }

    #[test]
    fn parse_update_no_where() {
        let sql = "UPDATE t SET v = 0";
        let dml = parse_dml(sql).unwrap().unwrap();
        match dml {
            DmlStatement::Update(up) => {
                assert_eq!(up.assignments.len(), 1);
                assert!(up.where_clause.is_none());
            }
            _ => panic!("expected Update"),
        }
    }

    #[test]
    fn parse_delete_with_ast_expr() {
        let sql = "DELETE FROM users WHERE id = 5";
        let dml = parse_dml(sql).unwrap().unwrap();
        match dml {
            DmlStatement::Delete(del) => {
                assert_eq!(del.table, "users");
                assert!(del.where_clause.is_some());
                match del.where_clause {
                    Some(Expr::Binary { op, .. }) => {
                        use crate::sql::ast::BinOp;
                        assert!(matches!(op, BinOp::Eq));
                    }
                    other => panic!("expected Binary in WHERE, got {other:?}"),
                }
            }
            _ => panic!("expected Delete"),
        }
    }

    #[test]
    fn parse_delete_returning() {
        let sql = "DELETE FROM t WHERE id = 5 RETURNING id, name";
        let dml = parse_dml(sql).unwrap().unwrap();
        match dml {
            DmlStatement::Delete(del) => {
                let returning = del.returning.expect("RETURNING clause");
                assert_eq!(returning.len(), 2);
                assert!(matches!(&returning[0], SelectItem::Column(c) if c == "id"));
                assert!(matches!(&returning[1], SelectItem::Column(c) if c == "name"));
            }
            _ => panic!("expected Delete"),
        }
    }

    #[test]
    fn parse_delete_no_where() {
        let sql = "DELETE FROM t";
        let dml = parse_dml(sql).unwrap().unwrap();
        match dml {
            DmlStatement::Delete(del) => assert!(del.where_clause.is_none()),
            _ => panic!("expected Delete"),
        }
    }

    #[test]
    fn parse_insert_null() {
        let sql = "INSERT INTO t (a, b) VALUES (1, NULL)";
        let dml = parse_dml(sql).unwrap().unwrap();
        match dml {
            DmlStatement::Insert(ins) => {
                assert_eq!(ins.values[0], vec!["1", "NULL"]);
            }
            _ => panic!("expected Insert"),
        }
    }

    #[test]
    fn not_dml_returns_none() {
        assert!(parse_dml("SELECT 1").unwrap().is_none());
        assert!(parse_dml("CREATE TABLE t (id INT)").unwrap().is_none());
    }
}
