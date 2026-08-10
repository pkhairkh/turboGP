//! # DML parser: INSERT / UPDATE / DELETE (Wave 4).
//!
//! Supports:
//! - `INSERT INTO table (col1, col2, ...) VALUES (v1, v2, ...), (v3, v4, ...)`
//! - `INSERT INTO table VALUES (v1, v2, ...)`
//! - `UPDATE table SET col = expr, ... WHERE condition`
//! - `DELETE FROM table WHERE condition`
//!
//! Expressions in SET/WHERE are stored as strings and evaluated by the
//! engine's existing expression evaluator.

use crate::sql::lexer::{tokenize, Token};

/// A parsed INSERT statement.
#[derive(Debug, Clone)]
pub struct Insert {
    /// Table name (may be schema.table).
    pub table: String,
    /// Optional column list. If None, all columns in table order.
    pub columns: Option<Vec<String>>,
    /// Values: one Vec per row, one String per column.
    pub values: Vec<Vec<String>>,
}

/// A parsed UPDATE statement.
#[derive(Debug, Clone)]
pub struct Update {
    pub table: String,
    /// (column_name, expression_string) pairs.
    pub assignments: Vec<(String, String)>,
    /// Optional WHERE clause as a token-string.
    pub where_clause: Option<String>,
}

/// A parsed DELETE statement.
#[derive(Debug, Clone)]
pub struct Delete {
    pub table: String,
    pub where_clause: Option<String>,
}

/// The result of parsing a DML statement.
#[derive(Debug, Clone)]
pub enum DmlStatement {
    Insert(Insert),
    Update(Update),
    Delete(Delete),
}

/// Parse a DML string. Returns None if not a DML statement.
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
    // Expect VALUES
    if pos >= tokens.len() {
        return Err("expected VALUES".into());
    }
    match &tokens[pos] {
        Token::Keyword(k) if k == "VALUES" => pos += 1,
        other => return Err(format!("expected VALUES, got {other:?}")),
    }
    // Parse value rows: (v1, v2, ...), (v3, v4, ...)
    let mut values = Vec::new();
    loop {
        if pos >= tokens.len() {
            return Err("expected ( after VALUES".into());
        }
        match &tokens[pos] {
            Token::LParen => pos += 1,
            Token::Semicolon | Token::EOF => break,
            other => return Err(format!("expected ( or end, got {other:?}")),
        }
        let mut row = Vec::new();
        loop {
            if pos >= tokens.len() {
                return Err("unterminated value list".into());
            }
            match &tokens[pos] {
                Token::RParen => {
                    pos += 1;
                    break;
                }
                Token::Comma => pos += 1,
                _ => {
                    let v = token_to_value_string(tokens, &mut pos)?;
                    row.push(v);
                }
            }
        }
        values.push(row);
        // Check for another row or end
        if pos >= tokens.len() {
            break;
        }
        match &tokens[pos] {
            Token::Comma => pos += 1,
            Token::Semicolon | Token::EOF => break,
            _ => break,
        }
    }
    Ok(Insert { table, columns, values })
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
    // Parse assignments: col = expr, col = expr, ...
    let mut assignments = Vec::new();
    loop {
        if pos >= tokens.len() {
            return Err("expected column name in SET".into());
        }
        let col = match &tokens[pos] {
            Token::Ident(s) => s.clone(),
            other => return Err(format!("expected column name in SET, got {other:?}")),
        };
        pos += 1;
        if pos >= tokens.len() {
            return Err("expected = after column name".into());
        }
        match &tokens[pos] {
            Token::Op(op) if op == "=" => pos += 1,
            other => return Err(format!("expected = after column name, got {other:?}")),
        }
        // Parse expression as a token-string until , or WHERE or end
        let expr = collect_expression(tokens, &mut pos, &[",", "WHERE"])?;
        assignments.push((col, expr));
        // Check for another assignment
        if pos < tokens.len() {
            match &tokens[pos] {
                Token::Comma => {
                    pos += 1;
                    continue;
                }
                _ => {}
            }
        }
        break;
    }
    // Optional WHERE
    let where_clause = if pos < tokens.len() {
        match &tokens[pos] {
            Token::Keyword(k) if k == "WHERE" => {
                pos += 1;
                Some(collect_rest(tokens, &mut pos))
            }
            _ => None,
        }
    } else {
        None
    };
    Ok(Update { table, assignments, where_clause })
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
                Some(collect_rest(tokens, &mut pos))
            }
            _ => None,
        }
    } else {
        None
    };
    Ok(Delete { table, where_clause })
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

fn collect_expression(
    tokens: &[Token],
    pos: &mut usize,
    stop_keywords: &[&str],
) -> Result<String, String> {
    let mut parts = Vec::new();
    while *pos < tokens.len() {
        match &tokens[*pos] {
            Token::Comma => break,
            Token::Semicolon | Token::EOF => break,
            Token::Keyword(k) if stop_keywords.contains(&k.as_str()) => break,
            Token::Keyword(k) => parts.push(k.clone()),
            Token::Ident(s) => parts.push(s.clone()),
            Token::Int(n) => parts.push(n.to_string()),
            Token::Float(f) => parts.push(f.to_string()),
            Token::String(s) => parts.push(format!("'{s}'")),
            Token::Op(op) => parts.push(op.clone()),
            Token::LParen => parts.push("(".into()),
            Token::RParen => parts.push(")".into()),
            Token::Hex(b) => parts
                .push(format!("x'{}'", b.iter().map(|b| format!("{:02x}", b)).collect::<String>())),
            _ => {}
        }
        *pos += 1;
    }
    Ok(parts.join(" "))
}

fn collect_rest(tokens: &[Token], pos: &mut usize) -> String {
    let mut parts = Vec::new();
    while *pos < tokens.len() {
        match &tokens[*pos] {
            Token::Semicolon | Token::EOF => break,
            Token::Keyword(k) => parts.push(k.clone()),
            Token::Ident(s) => parts.push(s.clone()),
            Token::Int(n) => parts.push(n.to_string()),
            Token::Float(f) => parts.push(f.to_string()),
            Token::String(s) => parts.push(format!("'{s}'")),
            Token::Op(op) => parts.push(op.clone()),
            Token::LParen => parts.push("(".into()),
            Token::RParen => parts.push(")".into()),
            Token::Comma => parts.push(",".into()),
            Token::Hex(b) => parts
                .push(format!("x'{}'", b.iter().map(|b| format!("{:02x}", b)).collect::<String>())),
            _ => {}
        }
        *pos += 1;
    }
    parts.join(" ")
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
    fn parse_update() {
        let sql = "UPDATE users SET name = 'Bob', active = 1 WHERE id = 5";
        let dml = parse_dml(sql).unwrap().unwrap();
        match dml {
            DmlStatement::Update(up) => {
                assert_eq!(up.table, "users");
                assert_eq!(up.assignments.len(), 2);
                assert_eq!(up.assignments[0], ("name".into(), "'Bob'".into()));
                assert_eq!(up.assignments[1], ("active".into(), "1".into()));
                assert!(up.where_clause.is_some());
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
    fn parse_delete() {
        let sql = "DELETE FROM users WHERE id = 5";
        let dml = parse_dml(sql).unwrap().unwrap();
        match dml {
            DmlStatement::Delete(del) => {
                assert_eq!(del.table, "users");
                assert!(del.where_clause.is_some());
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
