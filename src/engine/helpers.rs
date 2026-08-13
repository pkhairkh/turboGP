//! Helper functions for the engine module.
//!
//! Parsing utilities, value conversion, table/result conversion,
//! temporal table handling, PIVOT, JSON, UNION ALL, MERGE.

use super::*;

/// Strip the leading SQL keyword from a statement (Wave 3 — Agent C).
///
/// Given a trimmed SQL string like `"EXPLAIN SELECT * FROM t"` or
/// `"ANALYZE SELECT COUNT(*) FROM t"`, returns the substring after the
/// first keyword: `"SELECT * FROM t"` / `"SELECT COUNT(*) FROM t"`.
///
/// Used by `execute()` to extract the inner SQL for `EXPLAIN` and
/// `ANALYZE` after `classify_statement` has identified the verb. This
/// replaces the previous `&trimmed[8..]` byte-slicing approach, which was
/// fragile (it assumed the keyword was always exactly 8 bytes including
/// the trailing space).
pub(crate) fn strip_first_keyword(sql: &str) -> &str {
    let trimmed = sql.trim_start();
    // Find the end of the first run of non-whitespace characters.
    let end = trimmed
        .find(|c: char| c.is_whitespace())
        .unwrap_or(trimmed.len());
    // Skip the keyword and the whitespace after it.
    trimmed[end..].trim_start()
}

pub(crate) fn extract_string_literal(s: &str) -> Option<String> {
    let trimmed = s.trim();
    if !(trimmed.starts_with('\'') && trimmed.ends_with('\'') && trimmed.len() >= 2) {
        return None;
    }
    let inner = &trimmed[1..trimmed.len() - 1];
    // Handle the `''` escape (a literal single quote inside the string).
    Some(inner.replace("''", "'"))
}

/// Parse a value string from the DML parser into a u64 cell.
///
/// Supported formats:
/// - `"42"` → integer 42
/// - `"3.14"` → f64::to_bits(3.14)
/// - `"'hello'"` → xxh3 hash of "hello" (string columns are hashed)
/// - `"NULL"` → 0 (NULL is stored as 0; a proper null bitmap arrives in a later wave)
/// - `"x'0123'"` → first 8 bytes as u64
pub(crate) fn parse_value_cell(s: &str) -> u64 {
    use xxhash_rust::xxh3;
    let trimmed = s.trim();
    if trimmed == "NULL" {
        return 0;
    }
    // String literal: '...'
    if trimmed.starts_with('\'') && trimmed.ends_with('\'') && trimmed.len() >= 2 {
        let inner = &trimmed[1..trimmed.len() - 1];
        return xxh3::xxh3_64(inner.as_bytes());
    }
    // Hex literal: x'...'
    if trimmed.starts_with("x'") && trimmed.ends_with('\'') && trimmed.len() >= 3 {
        let hex = &trimmed[2..trimmed.len() - 1];
        let bytes: Vec<u8> = (0..hex.len())
            .step_by(2)
            .filter_map(|i| u8::from_str_radix(&hex[i..i + 2], 16).ok())
            .collect();
        let mut buf = [0u8; 8];
        for (i, &b) in bytes.iter().take(8).enumerate() {
            buf[i] = b;
        }
        return u64::from_le_bytes(buf);
    }
    // Float
    if trimmed.contains('.') || trimmed.contains('e') || trimmed.contains('E') {
        if let Ok(f) = trimmed.parse::<f64>() {
            return f.to_bits();
        }
    }
    // Integer
    if let Ok(n) = trimmed.parse::<i64>() {
        return n as u64;
    }
    if let Ok(n) = trimmed.parse::<u64>() {
        return n;
    }
    // Fallback: hash the string
    xxh3::xxh3_64(trimmed.as_bytes())
}

// -----------------------------------------------------------------------
// CHECK constraint expression evaluator (Task 3.5)
// -----------------------------------------------------------------------

/// Evaluate a CHECK constraint expression against a row's u64 cell values
/// (Task 3.5).
///
/// Returns `true` if the check **passes** (or is UNKNOWN, e.g. due to a
/// NULL operand); returns `false` if the check is **violated**.
///
/// Supported `Expr` variants (everything else returns `true` so unsupported
/// expressions never block a DML):
/// - `Expr::Binary { op: And|Or, .. }` — logical combinators (recurse).
/// - `Expr::Binary { op: Eq|NotEq|Lt|Gt|LtEq|GtEq, .. }` — comparison of
///   two operands (column-vs-literal, column-vs-column, literal-vs-literal).
/// - `Expr::Not(e)` — logical negation.
/// - `Expr::Paren(e)` — transparent.
/// - `Expr::Column(name)` in a boolean context — non-zero = true.
/// - `Expr::Literal(Value::Int(n))` — int literal (non-zero = true).
/// - `Expr::Literal(Value::Float(f))` — float literal (non-zero = true).
/// - `Expr::Literal(Value::Null)` — NULL → UNKNOWN → pass.
///
/// NULL handling (SQL standard): if a column referenced in a comparison
/// is NULL (per `null_mask`), the comparison is UNKNOWN, which means the
/// CHECK passes (a row is rejected only if the CHECK is FALSE).
///
/// Type handling: integer columns are interpreted as `i64` (so `x > 0`
/// correctly rejects `x = -1`, stored as `(-1i64) as u64` = `u64::MAX`).
/// Float columns are not specially handled here — when compared against
/// an `Int` literal, the cell is reinterpreted as `i64`. This works for
/// the common case of integer CHECK constraints; mixed int/float
/// comparisons may give incorrect results (a known limitation; the
/// evaluator prefers correctness for INT CHECKs over completeness).
pub(crate) fn eval_check_expr(
    expr: &crate::sql::ast::Expr,
    column_names: &[String],
    row_values: &[u64],
    null_mask: &[bool],
) -> bool {
    use crate::sql::ast::{BinOp, Expr, Value};
    match expr {
        Expr::Binary { left, op, right } => match op {
            BinOp::And => {
                eval_check_expr(left, column_names, row_values, null_mask)
                    && eval_check_expr(right, column_names, row_values, null_mask)
            }
            BinOp::Or => {
                eval_check_expr(left, column_names, row_values, null_mask)
                    || eval_check_expr(right, column_names, row_values, null_mask)
            }
            _ => {
                let lv = eval_check_operand(left, column_names, row_values, null_mask);
                let rv = eval_check_operand(right, column_names, row_values, null_mask);
                compare_operands(lv, rv, *op)
            }
        },
        Expr::Not(inner) => !eval_check_expr(inner, column_names, row_values, null_mask),
        Expr::Paren(inner) => eval_check_expr(inner, column_names, row_values, null_mask),
        Expr::Column(name) => {
            // Bare column in a boolean context: non-zero = true.
            let idx = column_names.iter().position(|c| c == name);
            match idx {
                Some(i) => {
                    if null_mask.get(i).copied().unwrap_or(false) {
                        true // NULL → UNKNOWN → pass
                    } else {
                        row_values.get(i).copied().unwrap_or(0) != 0
                    }
                }
                None => true, // unknown column — don't block
            }
        }
        Expr::Literal(Value::Int(n)) => *n != 0,
        Expr::Literal(Value::Float(f)) => *f != 0.0,
        Expr::Literal(Value::Null) => true,
        _ => true, // unsupported expr — don't block
    }
}

/// A typed operand extracted from an `Expr` for comparison in a CHECK.
enum CheckOperand {
    Null,
    Int(i64),
    Float(f64),
    /// String hashed via xxh3 (matches `parse_value_cell`).
    Str(u64),
}

fn eval_check_operand(
    expr: &crate::sql::ast::Expr,
    column_names: &[String],
    row_values: &[u64],
    null_mask: &[bool],
) -> CheckOperand {
    use crate::sql::ast::{Expr, Value};
    match expr {
        Expr::Column(name) => {
            let idx = column_names.iter().position(|c| c == name);
            match idx {
                Some(i) => {
                    if null_mask.get(i).copied().unwrap_or(false) {
                        CheckOperand::Null
                    } else {
                        // Interpret as i64. For FLOAT columns the cell is
                        // f64::to_bits; when compared against an Int literal,
                        // this gives a wrong-but-non-blocking answer in the
                        // rare mixed-type case (documented limitation).
                        CheckOperand::Int(row_values.get(i).copied().unwrap_or(0) as i64)
                    }
                }
                None => CheckOperand::Null, // unknown column → UNKNOWN → pass
            }
        }
        Expr::Literal(Value::Int(n)) => CheckOperand::Int(*n),
        Expr::Literal(Value::Float(f)) => CheckOperand::Float(*f),
        Expr::Literal(Value::Null) => CheckOperand::Null,
        Expr::Literal(Value::String(s)) => {
            use xxhash_rust::xxh3;
            CheckOperand::Str(xxh3::xxh3_64(s.as_bytes()))
        }
        Expr::Paren(inner) => eval_check_operand(inner, column_names, row_values, null_mask),
        _ => CheckOperand::Null, // unsupported → UNKNOWN → pass
    }
}

fn compare_operands(l: CheckOperand, r: CheckOperand, op: crate::sql::ast::BinOp) -> bool {
    use crate::sql::ast::BinOp;
    use CheckOperand::*;
    // NULL operand → UNKNOWN → pass (return true).
    if matches!(l, Null) || matches!(r, Null) {
        return true;
    }
    match (l, r) {
        (Int(a), Int(b)) => match op {
            BinOp::Eq => a == b,
            BinOp::NotEq => a != b,
            BinOp::Lt => a < b,
            BinOp::Gt => a > b,
            BinOp::LtEq => a <= b,
            BinOp::GtEq => a >= b,
            _ => true, // arithmetic / concat → don't block
        },
        (Float(a), Float(b)) => match op {
            BinOp::Eq => a == b,
            BinOp::NotEq => a != b,
            BinOp::Lt => a < b,
            BinOp::Gt => a > b,
            BinOp::LtEq => a <= b,
            BinOp::GtEq => a >= b,
            _ => true,
        },
        (Int(a), Float(b)) => match op {
            BinOp::Eq => (a as f64) == b,
            BinOp::NotEq => (a as f64) != b,
            BinOp::Lt => (a as f64) < b,
            BinOp::Gt => (a as f64) > b,
            BinOp::LtEq => (a as f64) <= b,
            BinOp::GtEq => (a as f64) >= b,
            _ => true,
        },
        (Float(a), Int(b)) => match op {
            BinOp::Eq => a == (b as f64),
            BinOp::NotEq => a != (b as f64),
            BinOp::Lt => a < (b as f64),
            BinOp::Gt => a > (b as f64),
            BinOp::LtEq => a <= (b as f64),
            BinOp::GtEq => a >= (b as f64),
            _ => true,
        },
        (Str(a), Str(b)) => match op {
            // String range comparisons are unsupported (cells are hashes,
            // so ordering is meaningless). Only equality is meaningful.
            BinOp::Eq => a == b,
            BinOp::NotEq => a != b,
            _ => true,
        },
        // Mixed string/int or string/float → unsupported, don't block.
        _ => true,
    }
}

/// Evaluate a simple WHERE clause against a table, returning a row mask.
///
/// Wave 50 fix (Bugs 4 & 5):
/// - Previously only supported `=` and split the WHERE string on
///   whitespace, which broke string literals containing spaces like
///   `'Alice Bob'`.
/// - Now uses the SQL lexer (`crate::sql::lexer::tokenize`) so quoted
///   strings with spaces round-trip correctly, and supports the full set
///   of comparison operators: `=`, `!=`, `<>`, `<`, `>`, `<=`, `>=`.
/// - Also supports `AND` / `OR` for combining predicates (left-associative).
pub(crate) fn eval_simple_where(table: &Table, where_str: &str) -> Result<Vec<bool>> {
    let n = table.row_count;
    if n == 0 {
        return Ok(Vec::new());
    }

    // Tokenize the WHERE clause so string literals with spaces, embedded
    // operators, etc. are correctly preserved as single tokens.
    let tokens = crate::sql::lexer::tokenize(where_str).map_err(Error::Parse)?;
    // Drop trailing EOF (and any leading WHERE keyword, in case the caller
    // passed the full predicate including `WHERE`).
    let tokens: Vec<crate::sql::lexer::Token> =
        tokens.into_iter().filter(|t| !matches!(t, crate::sql::lexer::Token::EOF)).collect();
    let tokens: Vec<crate::sql::lexer::Token> = if tokens
        .first()
        .and_then(|t| match t {
            crate::sql::lexer::Token::Keyword(k) if k.eq_ignore_ascii_case("WHERE") => Some(()),
            _ => None,
        })
        .is_some()
    {
        tokens[1..].to_vec()
    } else {
        tokens
    };

    if tokens.is_empty() {
        return Ok(vec![true; n]);
    }

    // Parse predicates of form: <col> <op> <value>, joined by AND/OR.
    // Each predicate produces a (col_idx, op, cell_value, is_string_literal, raw_string) tuple.
    #[derive(Clone)]
    struct Pred {
        col_idx: usize,
        op: String,
        cell: u64,
        // Original string literal (if the value was a quoted string), used
        // for string comparison when the column has a string sidecar.
        raw_string: Option<String>,
    }

    let mut predicates: Vec<Pred> = Vec::new();
    let mut operators: Vec<bool> = Vec::new(); // true = AND, false = OR
    let mut i = 0;
    while i < tokens.len() {
        match &tokens[i] {
            crate::sql::lexer::Token::Keyword(k) if k.eq_ignore_ascii_case("AND") => {
                operators.push(true);
                i += 1;
                continue;
            }
            crate::sql::lexer::Token::Keyword(k) if k.eq_ignore_ascii_case("OR") => {
                operators.push(false);
                i += 1;
                continue;
            }
            crate::sql::lexer::Token::LParen | crate::sql::lexer::Token::RParen => {
                // Task 5.1: skip parens — Expr::to_string() wraps binary
                // expressions in parens (e.g. "(id = 1)"), and we need to
                // handle this gracefully for simple col op value predicates.
                i += 1;
                continue;
            }
            _ => {}
        }

        // Expect: <col> <op> <value>
        let col_name = match &tokens[i] {
            crate::sql::lexer::Token::Ident(s) => s.clone(),
            crate::sql::lexer::Token::Keyword(k) => k.clone(), // tolerate keyword-as-identifier
            other => {
                return Err(Error::Other(format!(
                    "expected column name in WHERE clause, got {:?}",
                    other
                )))
            }
        };
        if i + 2 >= tokens.len() {
            return Err(Error::Other(format!("incomplete WHERE predicate near '{col_name}'")));
        }
        let op = match &tokens[i + 1] {
            crate::sql::lexer::Token::Op(s) => s.clone(),
            other => {
                return Err(Error::Other(format!(
                    "expected comparison operator after '{col_name}', got {:?}",
                    other
                )))
            }
        };
        if !matches!(op.as_str(), "=" | "!=" | "<>" | "<" | ">" | "<=" | ">=") {
            return Err(Error::Other(format!("unsupported WHERE operator '{op}' in DML WHERE")));
        }

        let col_idx = table
            .column_idx(&col_name)
            .ok_or_else(|| Error::NotFound(format!("column \"{col_name}\"")))?;

        // Extract the value cell. String literals get the original text
        // preserved so we can compare against the string sidecar if one
        // exists; everything else is parsed via parse_value_cell.
        let (cell, raw_string) = match &tokens[i + 2] {
            crate::sql::lexer::Token::String(s) => {
                // Quoted string. If the column has a string sidecar, we
                // keep the original text for direct comparison; otherwise
                // we hash it (matching parse_value_cell behaviour).
                let has_string_sidecar =
                    col_idx < table.string_columns.len() && table.string_columns[col_idx].is_some();
                if has_string_sidecar {
                    (0u64, Some(s.clone()))
                } else {
                    (parse_value_cell(&format!("'{}'", s)), None)
                }
            }
            crate::sql::lexer::Token::Int(v) => (*v as u64, None),
            crate::sql::lexer::Token::Float(f) => (f.to_bits(), None),
            crate::sql::lexer::Token::Hex(bytes) => {
                let mut buf = [0u8; 8];
                for (j, &b) in bytes.iter().take(8).enumerate() {
                    buf[j] = b;
                }
                (u64::from_le_bytes(buf), None)
            }
            crate::sql::lexer::Token::Keyword(k) if k.eq_ignore_ascii_case("NULL") => {
                // NULL in a WHERE predicate — treat as 0 cell. Callers
                // that need IS NULL / IS NOT NULL should use the
                // expression evaluator path.
                (0u64, None)
            }
            other => {
                return Err(Error::Other(format!(
                    "expected literal value in WHERE clause, got {:?}",
                    other
                )))
            }
        };

        predicates.push(Pred {
            col_idx,
            op: if op == "<>" { "!=".to_string() } else { op },
            cell,
            raw_string,
        });
        i += 3;
    }

    if predicates.is_empty() {
        return Ok(vec![true; n]);
    }

    // Evaluate each predicate per row.
    let mut per_pred_masks: Vec<Vec<bool>> = Vec::with_capacity(predicates.len());
    for p in &predicates {
        let col_idx = p.col_idx;
        let col = &table.columns[col_idx];

        // If we have the original string and the column has a string sidecar,
        // compare against the sidecar directly (lexicographic).
        if let Some(ref s) = p.raw_string {
            if col_idx < table.string_columns.len() {
                if let Some(ref sc) = table.string_columns[col_idx] {
                    let mask: Vec<bool> = (0..n)
                        .map(|i| {
                            let cell_str = sc.get(i);
                            match p.op.as_str() {
                                "=" => cell_str == s.as_str(),
                                "!=" => cell_str != s.as_str(),
                                "<" => cell_str < s.as_str(),
                                ">" => cell_str > s.as_str(),
                                "<=" => cell_str <= s.as_str(),
                                ">=" => cell_str >= s.as_str(),
                                _ => false,
                            }
                        })
                        .collect();
                    per_pred_masks.push(mask);
                    continue;
                }
            }
        }

        // Default: compare u64 cells.
        let val = p.cell;
        let mask: Vec<bool> = match p.op.as_str() {
            "=" => col.iter().map(|&c| c == val).collect(),
            "!=" => col.iter().map(|&c| c != val).collect(),
            "<" => col.iter().map(|&c| c < val).collect(),
            ">" => col.iter().map(|&c| c > val).collect(),
            "<=" => col.iter().map(|&c| c <= val).collect(),
            ">=" => col.iter().map(|&c| c >= val).collect(),
            _ => vec![false; n],
        };
        per_pred_masks.push(mask);
    }

    // Combine: start with first predicate, then AND/OR (left-associative).
    let mut result = per_pred_masks[0].clone();
    for (i, mask) in per_pred_masks[1..].iter().enumerate() {
        let is_and = operators.get(i).copied().unwrap_or(true);
        if is_and {
            for j in 0..n {
                result[j] = result[j] && mask[j];
            }
        } else {
            for j in 0..n {
                result[j] = result[j] || mask[j];
            }
        }
    }

    Ok(result)
}

// -----------------------------------------------------------------------
// CTE helper functions (Wave 6)
// -----------------------------------------------------------------------

/// Convert a QueryResult into a Table that can be registered in the catalog.
pub(crate) fn result_to_table(name: &str, result: &QueryResult) -> Table {
    let column_names: Vec<String> = result.columns.iter().map(|c| c.name.clone()).collect();
    let columns: Vec<std::sync::Arc<Vec<u64>>> =
        result.columns.iter().map(|c| std::sync::Arc::new(c.values.clone())).collect();
    let string_columns: Vec<Option<std::sync::Arc<crate::exec::fm_index::StringSearchColumn>>> =
        vec![None; result.columns.len()];
    Table {
        name: name.to_string(),
        columns,
        column_names,
        row_count: result.row_count,
        string_columns,
        null_bitmaps: vec![],
        i32_columns: vec![],
        schema: None,
        row_versions: Vec::new(),
    }
}

/// Compute how many rows in `result` are new (not already in `table`).
/// A row is "new" if its full column content doesn't match any existing
/// row in the table. This is O(result_rows × table_rows × ncols) —
/// expensive but correct for small CTEs.
pub(crate) fn compute_new_rows(table: &Table, result: &QueryResult) -> usize {
    if result.row_count == 0 {
        return 0;
    }
    let ncols = result.columns.len();
    let mut new_count = 0;
    for r_row in 0..result.row_count {
        let mut found = false;
        for t_row in 0..table.row_count {
            let mut matches = true;
            for col_idx in 0..ncols {
                let r_val = result.columns[col_idx].values.get(r_row).copied().unwrap_or(0);
                let t_val =
                    table.columns.get(col_idx).and_then(|c| c.get(t_row)).copied().unwrap_or(0);
                if r_val != t_val {
                    matches = false;
                    break;
                }
            }
            if matches {
                found = true;
                break;
            }
        }
        if !found {
            new_count += 1;
        }
    }
    new_count
}

/// Append all rows from a QueryResult to an existing Table. The table
/// must have the same number of columns as the result.
pub(crate) fn append_result_rows(table: &mut Table, result: &QueryResult) {
    for col_idx in 0..result.columns.len() {
        if col_idx < table.columns.len() {
            let col = std::sync::Arc::make_mut(&mut table.columns[col_idx]);
            col.extend_from_slice(&result.columns[col_idx].values);
        }
    }
    table.row_count += result.row_count;
}

pub(crate) fn substitute_proc_params(body: &str, args: &[String]) -> String {
    let mut result = body.to_string();
    // Positional substitution: @1, @2, ... → args[0], args[1], ...
    for (i, arg) in args.iter().enumerate() {
        let placeholder = format!("@{}", i + 1);
        result = result.replace(&placeholder, arg);
    }
    result
}


pub(crate) fn table_to_query_result(table: &Table) -> QueryResult {
    let columns: Vec<ResultColumn> = table
        .column_names
        .iter()
        .enumerate()
        .map(|(i, name)| ResultColumn {
            name: name.clone(),
            values: table.columns[i].to_vec(),
            string_values: None,
            type_oid: 0,
            null_mask: None,
        })
        .collect();
    QueryResult { columns, row_count: table.row_count, elapsed_us: 0 }
}

/// Convert a `QueryResult` back into a `Table` (round-trip after merge).
pub(crate) fn query_result_to_table(name: &str, qr: &QueryResult) -> Table {
    let columns: Vec<std::sync::Arc<Vec<u64>>> =
        qr.columns.iter().map(|c| std::sync::Arc::new(c.values.clone())).collect();
    let column_names: Vec<String> = qr.columns.iter().map(|c| c.name.clone()).collect();
    Table {
        name: name.to_string(),
        columns,
        column_names,
        row_count: qr.row_count,
        string_columns: vec![],
        null_bitmaps: vec![],
        i32_columns: vec![],
        schema: None,
        row_versions: Vec::new(),
    }
}

/// Parse `FOR SYSTEM_TIME AS OF <timestamp>` from a SQL string.
/// Returns (table_name, timestamp) if the clause is present.
///
/// SQL syntax: `SELECT ... FROM table_name FOR SYSTEM_TIME AS OF <ts>`
/// The table name appears between FROM and FOR SYSTEM_TIME.
pub(crate) fn parse_for_system_time(sql: &str) -> Option<(String, u64)> {
    let lower = sql.to_lowercase();
    let pos = lower.find("for system_time as of")?;
    // The timestamp is everything after "FOR SYSTEM_TIME AS OF" up to the
    // next non-digit character.
    let after = &sql[pos + "for system_time as of".len()..];
    let after_trimmed = after.trim_start();
    let ts_end = after_trimmed.find(|c: char| !c.is_ascii_digit()).unwrap_or(after_trimmed.len());
    if ts_end == 0 {
        return None;
    }
    let timestamp: u64 = after_trimmed[..ts_end].parse().ok()?;

    // The table name is between FROM and FOR SYSTEM_TIME. Look at the
    // substring before "FOR SYSTEM_TIME AS OF".
    let before = &sql[..pos];
    let before_lower = before.to_lowercase();
    let from_pos = before_lower.rfind("from ")?;
    let after_from = &before[from_pos + "from ".len()..];
    // The table name is the first whitespace-delimited token (optionally
    // followed by WHERE/ORDER/etc.).
    let table_name = after_from.split_whitespace().next()?.to_string();
    Some((table_name, timestamp))
}

/// Detect `WITH (SYSTEM_VERSIONING = ON)` in a CREATE TABLE SQL string
/// (case-insensitive) and return the table name. Used by `execute_inner`
/// to register the table in `self.temporals` (Wave 56d).
///
/// SQL syntax:
///   CREATE TABLE <name> (<cols>) WITH (SYSTEM_VERSIONING = ON)
///   CREATE TABLE <name> (<cols>) WITH (SYSTEM_VERSIONING=ON)
///
/// Returns None if the SYSTEM_VERSIONING clause is not present or the
/// table name can't be extracted.
pub(crate) fn extract_temporal_table_name(sql: &str) -> Option<String> {
    let lower = sql.to_lowercase();
    // Look for "system_versioning" — accept both `SYSTEM_VERSIONING = ON`
    // and `SYSTEM_VERSIONING=ON` (no spaces around =).
    if !lower.contains("system_versioning") {
        return None;
    }
    // Check that ON follows (allow whitespace and optional = sign).
    let sv_pos = lower.find("system_versioning")?;
    let after_sv = &lower[sv_pos + "system_versioning".len()..];
    let after_sv_trimmed = after_sv.trim_start();
    // Optional '='.
    let after_eq =
        if after_sv_trimmed.starts_with('=') { &after_sv_trimmed[1..] } else { after_sv_trimmed };
    let after_eq_trimmed = after_eq.trim_start();
    if !after_eq_trimmed.starts_with("on") {
        return None;
    }
    // Extract the table name: the first identifier after "CREATE TABLE".
    let create_pos = lower.find("create table")?;
    let after_create = &sql[create_pos + "create table".len()..];
    let after_create_trimmed = after_create.trim_start();
    // Optional IF NOT EXISTS.
    let after_ifne = if after_create_trimmed.to_lowercase().starts_with("if not exists") {
        &after_create_trimmed["if not exists".len()..].trim_start()
    } else {
        after_create_trimmed
    };
    // The table name is the first identifier (up to whitespace, '.', or '(').
    let end = after_ifne.find(|c: char| c.is_whitespace() || c == '.' || c == '(')?;
    let name = &after_ifne[..end];
    if name.is_empty() {
        return None;
    }
    Some(name.to_string())
}

/// Convert temporal-table rows (Vec<Vec<u64>>) into a QueryResult.
pub(crate) fn rows_to_query_result(
    rows: &[Vec<u64>],
    column_names: &[String],
    start: &Instant,
) -> QueryResult {
    let row_count = rows.len();
    let n_cols = column_names.len();
    let columns: Vec<ResultColumn> = (0..n_cols)
        .map(|i| {
            let values: Vec<u64> = rows.iter().map(|r| r.get(i).copied().unwrap_or(0)).collect();
            ResultColumn {
                name: column_names[i].clone(),
                values,
                string_values: None,
                type_oid: 0,
                null_mask: None,
            }
        })
        .collect();
    QueryResult { columns, row_count, elapsed_us: start.elapsed().as_micros() as u64 }
}

/// Apply window functions to a QueryResult (Wave 53 wiring for
/// exec/window.rs). Detects `SelectItem::Window` items in the query and
/// appends a new ResultColumn for each.
pub(crate) fn apply_window_functions(
    result: &QueryResult,
    query: &crate::sql::parser::SelectQuery,
) -> QueryResult {
    use crate::exec::window::{
        count_over, dense_rank, parse_window_spec, rank, row_number, sum_over,
    };
    use crate::sql::parser::SelectItem;

    let mut new_cols: Vec<ResultColumn> = result.columns.clone();
    for item in &query.select {
        if let SelectItem::Window { func, arg, over_spec, alias } = item {
            let spec = match parse_window_spec(over_spec) {
                Ok(s) => s,
                Err(_) => continue,
            };
            let func_upper = func.to_uppercase();
            let name = alias.clone().unwrap_or_else(|| func.to_lowercase());
            let values = match func_upper.as_str() {
                "ROW_NUMBER" => row_number(result, &spec),
                "RANK" => rank(result, &spec),
                "DENSE_RANK" => dense_rank(result, &spec),
                "SUM" => sum_over(result, arg, &spec),
                "COUNT" => count_over(result, &spec),
                _ => continue,
            };
            new_cols.push(ResultColumn {
                name,
                values,
                string_values: None,
                type_oid: 0,
                null_mask: None,
            });
        }
    }
    QueryResult { columns: new_cols, row_count: result.row_count, elapsed_us: result.elapsed_us }
}

/// Stub for parsing PIVOT/UNPIVOT from QueryExtensions. The current
/// `QueryExtensions` type doesn't carry pivot specs, so this always
/// returns None. PIVOT is now wired through `parse_pivot_clause` which
/// detects the PIVOT keyword directly in the SQL string (Wave 56b).
pub(crate) fn extensions_pivot(_ext: &crate::sql::extensions::QueryExtensions) -> Option<PivotSpec> {
    None
}

/// A parsed PIVOT specification (Wave 53).
pub(crate) struct PivotSpec {
    pub(crate) group_col: String,
    pub(crate) pivot_col: String,
    pub(crate) value_col: String,
    pub(crate) pivot_values: Vec<String>,
    pub(crate) agg: String,
}

/// A parsed PIVOT clause extracted from a SQL string (Wave 56b).
/// `group_col` is auto-detected at apply time (see execute_inner).
///
/// Production Wiring Wave 7: the parsing logic that produced this struct
/// (`parse_pivot_clause` + `strip_pivot_clause`) has been **deleted**.
/// The formal PIVOT AST now lives in [`crate::sql::ast::PivotClause`] and
/// the parsing logic lives in [`crate::sql::pivot`]. Callers should use
/// the new module directly. The local `PivotClause` struct is retained
/// only for the [`PivotSpec`] group_col-detected shape used by
/// [`apply_pivot`]; the engine constructs it from the formal AST.
pub(crate) struct PivotClause {
    pub(crate) agg: String,
    pub(crate) value_col: String,
    pub(crate) pivot_col: String,
    pub(crate) pivot_values: Vec<String>,
}

/// Apply a PIVOT transformation to a QueryResult (Wave 53 wiring for
/// exec/pivot.rs).
pub(crate) fn apply_pivot(result: &QueryResult, spec: &PivotSpec) -> QueryResult {
    crate::exec::pivot::pivot(
        result,
        &spec.group_col,
        &spec.pivot_col,
        &spec.value_col,
        &spec.pivot_values,
        &spec.agg,
    )
}

// -----------------------------------------------------------------------
// Wave 56c: JSON_VALUE / JSON_QUERY wiring.
// -----------------------------------------------------------------------

/// Check whether a SQL string contains a `JSON_VALUE(` or `JSON_QUERY(` call
/// (case-insensitive). Used by `execute_inner` to decide whether to intercept
/// the query for JSON post-processing.
pub(crate) fn contains_json_value_call(sql: &str) -> bool {
    let lower = sql.to_lowercase();
    lower.contains("json_value(") || lower.contains("json_query(")
}

// -----------------------------------------------------------------------
// Wave 7 (Task 7.1): Formal UNION / UNION ALL dispatch via `SetQuery`.
//
// The previous `split_union_all` string hack has been deleted. UNION and
// UNION ALL now go through the formal `parse_set` parser, which produces
// a `SetQuery::Union` / `SetQuery::UnionAll` AST. `execute_set_query`
// walks that tree and concatenates the results of each leaf SELECT.
// -----------------------------------------------------------------------

/// Try to parse a SQL string as a top-level set operation
/// (`UNION` / `UNION ALL`). Returns `Some((set, ext))` if the SQL parses
/// as a `SetQuery::Union` or `SetQuery::UnionAll`, otherwise `None`.
///
/// This is used by `execute_inner` to dispatch UNION/UNION ALL via the
/// formal `SetQuery` AST instead of the previous `split_union_all` string
/// hack. Plain `SELECT` statements (without a set operation) return `None`
/// so the caller falls through to the normal SELECT path.
///
/// INTERSECT and EXCEPT are valid set operations but are not yet wired
/// through `execute_set_query`; this function returns `None` for them so
/// the caller can fall back to the interpreter.
pub(crate) fn try_parse_as_set_query(
    sql: &str,
) -> Option<(crate::sql::parser::SetQuery, crate::sql::extensions::QueryExtensions)> {
    use crate::sql::extensions::parse_extensions_and_strip;
    use crate::sql::lexer::tokenize;
    use crate::sql::parser::{parse_set, SetQuery};

    let tokens = tokenize(sql).ok()?;
    let (ext, stripped) = parse_extensions_and_strip(tokens).ok()?;
    let set = parse_set(stripped).ok()?;
    match &set {
        SetQuery::Union(_, _) | SetQuery::UnionAll(_, _) => Some((set, ext)),
        _ => None,
    }
}

// -----------------------------------------------------------------------
// Wave 7 (Task 7.2): Formal MERGE dispatch via `MergeStmt` AST.
//
// The previous `parse_merge` string-scan hack has been deleted. MERGE
// now goes through the formal `parse_merge_stmt` parser in
// `src/sql/parser.rs`, which produces a `MergeStmt` AST. This AST is
// then converted to `crate::exec::merge::Merge` via
// [`merge_stmt_to_merge`] for execution by the existing `execute_merge`
// executor.
// -----------------------------------------------------------------------

/// Try to parse a SQL string as a MERGE statement. Returns `Some(MergeStmt)`
/// if the SQL is a valid MERGE statement, otherwise `None`.
///
/// Used by `execute_inner` to dispatch MERGE via the formal `MergeStmt`
/// AST instead of the previous `parse_merge` string hack. Non-MERGE
/// statements (SELECT, INSERT, etc.) return `None` so the caller falls
/// through to the appropriate handler.
pub(crate) fn try_parse_merge_stmt(sql: &str) -> Option<crate::sql::parser::MergeStmt> {
    use crate::sql::lexer::tokenize;
    use crate::sql::parser::parse_merge_stmt;
    let tokens = tokenize(sql).ok()?;
    parse_merge_stmt(tokens).ok()
}

/// Convert a formal [`MergeStmt`](crate::sql::parser::MergeStmt) AST to
/// the executor's [`Merge`](crate::exec::merge::Merge) struct.
///
/// This bridges the formal parser (which produces `MergeStmt`) and the
/// existing `execute_merge` function (which consumes `Merge`). The
/// conversion:
///
/// 1. Computes `source_rows: Vec<(join_key, full_row)>` from the parsed
///    VALUES list, using the join source column index to extract the
///    join key for each row.
/// 2. Maps `MergeAction::Update` / `Delete` / `Insert` to the executor's
///    `MergeAction::Update(Vec)` / `Delete` / `Insert(Vec, Vec)`.
/// 3. Drops all but the first WHEN MATCHED and first WHEN NOT MATCHED
///    clause (the existing executor only handles one of each).
/// 4. Table and subquery sources are not yet wired through — they
///    produce empty `source_rows` (TODO: read from catalog / run subquery).
pub(crate) fn merge_stmt_to_merge(stmt: &crate::sql::parser::MergeStmt) -> crate::exec::merge::Merge {
    use crate::exec::merge::{Merge, MergeAction as ExecMergeAction};
    use crate::sql::parser::{MergeAction, MergeSource};

    let (source_rows, source_col_names) = match &stmt.source {
        MergeSource::Values { rows, col_names } => {
            let src_idx = col_names
                .iter()
                .position(|c| c.eq_ignore_ascii_case(&stmt.on.source_col));
            let source_rows: Vec<(String, Vec<String>)> = rows
                .iter()
                .map(|vals| {
                    let key = src_idx
                        .and_then(|i| vals.get(i).cloned())
                        .unwrap_or_default();
                    (key, vals.clone())
                })
                .collect();
            (source_rows, col_names.clone())
        }
        MergeSource::Table(_) | MergeSource::Subquery(_) => {
            // Table/subquery sources would require reading from the catalog
            // or running the subquery — not yet wired. Return empty so the
            // executor's no-source-row branch is taken (WHEN NOT MATCHED
            // fires for every target row, WHEN MATCHED never fires).
            (Vec::new(), Vec::new())
        }
    };

    let when_matched = stmt.when_matched.first().map(convert_merge_action);
    let when_not_matched_by_target = stmt.when_not_matched.first().map(convert_merge_action);

    Merge {
        target: stmt.target.clone(),
        source_rows,
        source_col_names,
        join_target_col: stmt.on.target_col.clone(),
        join_source_col: stmt.on.source_col.clone(),
        when_matched,
        when_not_matched_by_source: None,
        when_not_matched_by_target,
    }
}

/// Convert a parser `MergeAction` to the executor's `MergeAction`.
fn convert_merge_action(a: &crate::sql::parser::MergeAction) -> crate::exec::merge::MergeAction {
    use crate::exec::merge::MergeAction as ExecMergeAction;
    use crate::sql::parser::MergeAction;
    match a {
        MergeAction::Update { sets } => ExecMergeAction::Update(sets.clone()),
        MergeAction::Delete => ExecMergeAction::Delete,
        MergeAction::Insert { columns, values } => {
            ExecMergeAction::Insert(columns.clone(), values.clone())
        }
    }
}

/// Concatenate two QueryResults into one (UNION ALL). The result has the
/// columns from the left result; the right result's values are appended.
/// Column names are taken from the left result.
pub(crate) fn concatenate_results(left: QueryResult, right: QueryResult, start: &Instant) -> QueryResult {
    let total_rows = left.row_count + right.row_count;
    let n_cols = left.columns.len();
    let mut columns: Vec<ResultColumn> = left
        .columns
        .into_iter()
        .enumerate()
        .map(|(i, mut c)| {
            // Append the right result's values for this column.
            if i < right.columns.len() {
                c.values.extend(right.columns[i].values.iter().copied());
                // Merge string_values if both have them.
                if let Some(ref mut left_sv) = c.string_values {
                    if let Some(ref right_sv) = right.columns[i].string_values {
                        left_sv.extend(right_sv.iter().cloned());
                    } else {
                        // Right has no strings — pad with empty strings.
                        left_sv.extend(std::iter::repeat(String::new()).take(right.row_count));
                    }
                }
            }
            c
        })
        .collect();
    // If right has more columns than left (shouldn't happen for a valid UNION),
    // pad with empty columns.
    while columns.len() < n_cols {
        columns.push(ResultColumn {
            name: format!("col_{}", columns.len()),
            values: vec![0; total_rows],
            string_values: None,
            type_oid: 0,
            null_mask: None,
        });
    }
    QueryResult { columns, row_count: total_rows, elapsed_us: start.elapsed().as_micros() as u64 }
}

// -----------------------------------------------------------------------
// Wave 60d: SELECT DISTINCT wiring.
// -----------------------------------------------------------------------

/// Deduplicate the rows of a QueryResult. Two rows are considered duplicates
/// if they have the same u64 values in every column. The first occurrence is
/// kept; subsequent duplicates are dropped.
pub(crate) fn deduplicate_rows(result: QueryResult) -> QueryResult {
    if result.row_count <= 1 {
        return result;
    }

    // W16-T1: Sort-based dedup for single-column case.
    // The HashSet<Vec<u64>> approach allocates a Vec per row (100M allocs
    // for Q39) and does random-access HashSet insertions. For single-column
    // results, sort + remove consecutive duplicates is O(n log n) with
    // sequential access — much faster.
    if result.columns.len() == 1 && result.columns[0].string_values.is_none() {
        use rayon::prelude::*;
        let col = &result.columns[0];
        let n = result.row_count;
        // Build (value, original_index) pairs, sort by value.
        let mut pairs: Vec<(u64, u32)> = (0..n)
            .map(|i| (col.values.get(i).copied().unwrap_or(0), i as u32))
            .collect();
        pairs.par_sort_unstable_by_key(|(v, _)| *v);
        // Keep first occurrence of each value (preserve original order among
        // duplicates by keeping the smallest original index per value).
        let mut keep_indices: Vec<usize> = Vec::new();
        if !pairs.is_empty() {
            let mut cur_val = pairs[0].0;
            let mut cur_min_idx = pairs[0].1 as usize;
            for &(v, idx) in &pairs[1..] {
                if v == cur_val {
                    if (idx as usize) < cur_min_idx {
                        cur_min_idx = idx as usize;
                    }
                } else {
                    keep_indices.push(cur_min_idx);
                    cur_val = v;
                    cur_min_idx = idx as usize;
                }
            }
            keep_indices.push(cur_min_idx);
        }
        // Sort keep_indices to preserve original row order.
        keep_indices.sort_unstable();
        let new_row_count = keep_indices.len();
        let columns: Vec<ResultColumn> = result
            .columns
            .into_iter()
            .map(|mut c| {
                let new_values: Vec<u64> =
                    keep_indices.iter().map(|&i| c.values.get(i).copied().unwrap_or(0)).collect();
                c.values = new_values;
                c
            })
            .collect();
        return QueryResult { columns, row_count: new_row_count, elapsed_us: result.elapsed_us };
    }

    use std::collections::HashSet;
    let mut seen: HashSet<Vec<u64>> = HashSet::new();
    let mut keep_indices: Vec<usize> = Vec::with_capacity(result.row_count);
    for row in 0..result.row_count {
        let key: Vec<u64> =
            result.columns.iter().map(|c| c.values.get(row).copied().unwrap_or(0)).collect();
        if seen.insert(key) {
            keep_indices.push(row);
        }
    }
    if keep_indices.len() == result.row_count {
        return result; // no duplicates
    }
    let new_row_count = keep_indices.len();
    let columns: Vec<ResultColumn> = result
        .columns
        .into_iter()
        .map(|mut c| {
            let new_values: Vec<u64> =
                keep_indices.iter().map(|&i| c.values.get(i).copied().unwrap_or(0)).collect();
            c.values = new_values;
            if let Some(ref mut sv) = c.string_values {
                let new_sv: Vec<String> =
                    keep_indices.iter().map(|&i| sv.get(i).cloned().unwrap_or_default()).collect();
                *sv = new_sv;
            }
            if let Some(ref mut bm) = c.null_mask {
                let new_bm: Vec<bool> =
                    keep_indices.iter().map(|&i| bm.get(i).copied().unwrap_or(false)).collect();
                *bm = new_bm;
            }
            c
        })
        .collect();
    QueryResult { columns, row_count: new_row_count, elapsed_us: result.elapsed_us }
}

/// A parsed JSON_VALUE / JSON_QUERY call extracted from a SQL string.
pub(crate) struct JsonValueCall {
    /// Byte offset in the original SQL where the call begins (the 'J' of
    /// JSON_VALUE / JSON_QUERY).
    pub(crate) start: usize,
    /// Byte offset one past the closing ')' of the call (or past the alias
    /// if one was present).
    pub(crate) end: usize,
    /// The column name argument (first arg).
    pub(crate) col_name: String,
    /// The JSON path argument (second arg, without quotes).
    pub(crate) path: String,
    /// Whether this is JSON_QUERY (true) or JSON_VALUE (false).
    pub(crate) is_query: bool,
    /// Optional `AS alias` that immediately follows the call (consumed from
    /// the SQL during rewriting).
    pub(crate) alias: Option<String>,
    /// 0-indexed position of this call in the SELECT list (count of top-level
    /// commas between SELECT and the call's byte position). Used to find the
    /// corresponding result column after execution, since the basic parser
    /// discards column aliases.
    pub(crate) select_position: usize,
}

/// Extract all JSON_VALUE / JSON_QUERY calls from a SQL string. Returns one
/// entry per call, in order of appearance. Each entry carries the byte range
/// so the caller can rewrite the SQL.
pub(crate) fn extract_json_value_calls(sql: &str) -> Vec<JsonValueCall> {
    let lower = sql.to_lowercase();
    // Find the SELECT keyword position (to compute select_position).
    let select_pos = lower.find("select ").or_else(|| lower.find("select\n"));
    let mut calls = Vec::new();
    let mut search_from = 0;
    loop {
        // Find the next "json_value(" or "json_query(".
        let jv_pos = lower[search_from..].find("json_value(").map(|p| p + search_from);
        let jq_pos = lower[search_from..].find("json_query(").map(|p| p + search_from);
        let (pos, is_query) = match (jv_pos, jq_pos) {
            (Some(p), Some(q)) => {
                if p <= q {
                    (p, false)
                } else {
                    (q, true)
                }
            }
            (Some(p), None) => (p, false),
            (None, Some(q)) => (q, true),
            (None, None) => break,
        };
        // Walk forward from `pos` to find the matching close paren.
        let after_open = lower[pos..].find('(').unwrap() + 1;
        let mut depth = 1i32;
        let mut cur = pos + after_open;
        let bytes = sql.as_bytes();
        let mut in_str = false;
        let mut close = None;
        while cur < bytes.len() {
            let c = bytes[cur] as char;
            if in_str {
                if c == '\'' {
                    in_str = false;
                }
            } else {
                match c {
                    '\'' => in_str = true,
                    '(' => depth += 1,
                    ')' => {
                        depth -= 1;
                        if depth == 0 {
                            close = Some(cur);
                            break;
                        }
                    }
                    _ => {}
                }
            }
            cur += 1;
        }
        let close = match close {
            Some(c) => c,
            None => break,
        };
        // The args are between after_open (relative to pos) and close.
        let args_str = &sql[pos + after_open..close];
        // Parse the two arguments: col_name, 'path'.
        let (col_name, path) = match parse_json_value_args(args_str) {
            Some(p) => p,
            None => {
                search_from = close + 1;
                continue;
            }
        };
        // Look for an optional `AS alias` after the close paren.
        let mut after = close + 1;
        let rest = &sql[after..];
        let rest_trimmed = rest.trim_start();
        let leading_ws = rest.len() - rest_trimmed.len();
        let alias = if rest_trimmed.to_uppercase().starts_with("AS ") {
            let after_as = &rest_trimmed["AS ".len()..];
            let alias_len =
                after_as.chars().take_while(|c| c.is_alphanumeric() || *c == '_').count();
            if alias_len > 0 {
                let alias = after_as[..alias_len].to_string();
                after += leading_ws + "AS ".len() + alias_len;
                Some(alias)
            } else {
                None
            }
        } else {
            None
        };
        // Compute the 0-indexed position of this call in the SELECT list:
        // count top-level commas between SELECT and the call's byte position.
        let select_position =
            if let Some(sp) = select_pos { count_top_level_commas(&lower[sp..pos]) } else { 0 };
        calls.push(JsonValueCall {
            start: pos,
            end: after,
            col_name,
            path,
            is_query,
            alias,
            select_position,
        });
        search_from = after;
    }
    calls
}

/// Count top-level commas in a SQL substring (commas not inside parentheses
/// or string literals). Used to determine a JSON_VALUE call's position in
/// the SELECT list.
pub(crate) fn count_top_level_commas(s: &str) -> usize {
    let mut depth = 0i32;
    let mut in_str = false;
    let mut count = 0;
    for c in s.chars() {
        if in_str {
            if c == '\'' {
                in_str = false;
            }
        } else {
            match c {
                '\'' => in_str = true,
                '(' => depth += 1,
                ')' => depth -= 1,
                ',' if depth == 0 => count += 1,
                _ => {}
            }
        }
    }
    count
}

/// Parse the arguments of a JSON_VALUE / JSON_QUERY call: `<col>, '<path>'`.
/// Returns (col_name, path) or None if the args don't match the expected shape.
pub(crate) fn parse_json_value_args(args: &str) -> Option<(String, String)> {
    // Split on the first comma that's not inside a string.
    let mut in_str = false;
    let mut comma_pos = None;
    for (i, c) in args.char_indices() {
        match c {
            '\'' => in_str = !in_str,
            ',' if !in_str => {
                comma_pos = Some(i);
                break;
            }
            _ => {}
        }
    }
    let comma_pos = comma_pos?;
    let col_name = args[..comma_pos].trim().to_string();
    let path_part = args[comma_pos + 1..].trim();
    // path_part should be '...' — strip the quotes.
    let path = if path_part.starts_with('\'') && path_part.ends_with('\'') && path_part.len() >= 2 {
        path_part[1..path_part.len() - 1].to_string()
    } else {
        return None;
    };
    if col_name.is_empty() || path.is_empty() {
        return None;
    }
    Some((col_name, path))
}

pub(crate) fn default_cell_for_type(col_def: &crate::sql::ColumnDef, _row_count: usize) -> u64 {
    use crate::sql::ColumnType;
    // If a DEFAULT literal is present, parse it.
    if let Some(ref default) = col_def.default {
        let trimmed = default.trim();
        // String literal — hash it.
        if trimmed.starts_with('\'') && trimmed.ends_with('\'') && trimmed.len() >= 2 {
            let inner = &trimmed[1..trimmed.len() - 1];
            return xxhash_rust::xxh3::xxh3_64(inner.as_bytes());
        }
        // Integer literal.
        if let Ok(n) = trimmed.parse::<i64>() {
            return n as u64;
        }
        // Float literal.
        if let Ok(f) = trimmed.parse::<f64>() {
            return f.to_bits();
        }
        // NULL keyword.
        if trimmed.eq_ignore_ascii_case("null") {
            return 0;
        }
        // Fall through to type-based default.
    }
    match col_def.col_type {
        ColumnType::Int
        | ColumnType::BigInt
        | ColumnType::SmallInt
        | ColumnType::TinyInt
        | ColumnType::Bit
        | ColumnType::Boolean
        | ColumnType::Date
        | ColumnType::Timestamp
        | ColumnType::Uuid => 0,
        ColumnType::Float
        | ColumnType::Real
        | ColumnType::Decimal(_, _)
        | ColumnType::Numeric(_, _) => 0.0f64.to_bits(),
        ColumnType::Varchar(_)
        | ColumnType::Nvarchar(_)
        | ColumnType::Text
        | ColumnType::Json
        | ColumnType::Array(_)
        | ColumnType::Bytea
        | ColumnType::Enum(_) => {
            // Empty string → hash of empty bytes (consistent with INSERT
            // of '' which goes through parse_value_cell).
            xxhash_rust::xxh3::xxh3_64(b"")
        }
    }
}

/// Extract a `col = literal` predicate from an Expr (Wave 66).
///
/// Returns `Some((col_name, value_cell))` if the expr is a simple
/// equality between a column and a literal (in either order). Returns
/// `None` for any other shape (AND/OR, range, LIKE, etc.).
///
/// The literal is converted to a u64 cell using the same rules as the
/// executor's `literal_to_u64` (int → as u64, float → to_bits, string →
/// parse as int or xxh3 hash).
pub(crate) fn extract_eq_predicate(expr: &crate::sql::parser::Expr) -> Option<(String, u64)> {
    use crate::sql::parser::{Expr, Value};
    match expr {
        Expr::Binary { left, op, right } if *op == crate::sql::parser::BinOp::Eq => {
            // Try left=column, right=literal.
            if let (Expr::Column(name), Expr::Literal(val)) = (left.as_ref(), right.as_ref()) {
                return Some((name.clone(), literal_to_cell(val)?));
            }
            // Try right=column, left=literal.
            if let (Expr::Column(name), Expr::Literal(val)) = (right.as_ref(), left.as_ref()) {
                return Some((name.clone(), literal_to_cell(val)?));
            }
            None
        }
        _ => None,
    }
}

/// Convert a parsed literal Value to a u64 cell (Wave 66 helper).
pub(crate) fn literal_to_cell(val: &crate::sql::parser::Value) -> Option<u64> {
    use crate::sql::parser::Value;
    match val {
        Value::Int(i) => Some(*i as u64),
        Value::Float(f) => Some(f.to_bits()),
        Value::String(s) => {
            // Try parsing as an integer first (e.g. WHERE id = '42').
            if let Ok(n) = s.parse::<i64>() {
                return Some(n as u64);
            }
            // Otherwise hash the string (matches the executor's behavior
            // for string equality on hashed columns).
            Some(xxhash_rust::xxh3::xxh3_64(s.as_bytes()))
        }
        Value::Hex(bytes) => {
            let v =
                bytes.iter().enumerate().fold(0u64, |acc, (i, &b)| acc | ((b as u64) << (8 * i)));
            Some(v)
        }
        Value::Date(d) => Some(*d as u64),
        Value::Null => None,
    }
}

// ---------------------------------------------------------------------------
// DML helper impls for `QueryEngine` (moved from `src/engine/mod.rs` in
// Task 8.2-fix to satisfy the 2000-LOC file-size limit).
//
// These three methods (`materialize_views_in_sql`, `execute_merge_stmt`,
// `execute_with_json_value`) used to live in mod.rs as private impl
// blocks. They are declared `pub(crate)` here so mod.rs (and the rest
// of the crate) can still call them via `self.<method>`.
//
// Note: `QueryEngine::execute_inner` was bumped from `fn` to
// `pub(crate) fn` in mod.rs to support the cross-module call.
// ---------------------------------------------------------------------------

impl QueryEngine {
    /// Expand any views referenced in `sql` into real catalog tables by
    /// running the view's `select_sql` and registering the result under
    /// the view's name. Returns the SQL unchanged (the view tables are
    /// already in the catalog by the time the caller runs the SQL).
    pub(crate) fn materialize_views_in_sql(&mut self, sql: &str) -> String {
        let lower = sql.to_lowercase();
        // Collect view names that appear in the SQL before mutating self.
        let view_names: Vec<String> = self
            .views
            .names()
            .into_iter()
            .map(|s| s.to_string())
            .filter(|view_name| {
                let pattern = format!("from {}", view_name.to_lowercase());
                lower.contains(&pattern)
            })
            .collect();
        // Now materialize each view. We collect (name, select_sql) pairs
        // first to release the immutable borrow on self.views before we
        // call self.execute_inner (which needs &mut self).
        let view_specs: Vec<(String, String)> = view_names
            .into_iter()
            .filter_map(|name| self.views.get(&name).map(|v| (name, v.select_sql.clone())))
            .collect();
        for (view_name, select_sql) in view_specs {
            if let Ok(result) = self.execute_inner(&select_sql, &Instant::now(), None) {
                let table = result_to_table(&view_name, &result);
                self.catalog.register(table);
            }
        }
        sql.to_string()
    }

    /// Execute a MERGE statement against a catalog table (Wave 53 wiring
    /// for exec/merge.rs). The target table is loaded into a QueryResult,
    /// `execute_merge` is applied, and the result is written back to the
    /// catalog.
    pub(crate) fn execute_merge_stmt(
        &mut self,
        merge: crate::exec::merge::Merge,
        start: &Instant,
    ) -> Result<QueryResult> {
        let target_name = merge.target.clone();
        // Load the target table into a QueryResult.
        let table = self
            .catalog
            .get(&target_name)
            .ok_or_else(|| Error::NotFound(format!("MERGE target table \"{target_name}\"")))?
            .clone();
        let mut qr = table_to_query_result(&table);

        let merge_result = crate::exec::merge::execute_merge(&mut qr, &merge);

        // Write the mutated QueryResult back into the catalog table.
        let new_table = query_result_to_table(&target_name, &qr);
        self.catalog.register(new_table);

        let mut result = QueryResult::empty();
        result.row_count = merge_result.inserted + merge_result.updated + merge_result.deleted;
        result.elapsed_us = start.elapsed().as_micros() as u64;
        Ok(result)
    }

    /// Execute a parsed `SetQuery` tree (Wave 7 — formal UNION/UNION ALL
    /// support, replacing the `split_union_all` string hack).
    ///
    /// Recursively walks the tree:
    /// - `Select(q)` leaf → [`Self::execute_select_query`] (calls
    ///   `execute_select` directly with the parsed `SelectQuery`).
    /// - `UnionAll(left, right)` → execute both sides, concatenate via
    ///   [`concatenate_results`].
    /// - `Union(left, right)` → execute both sides, concatenate, then
    ///   deduplicate via [`deduplicate_rows`].
    /// - `Intersect` / `Except` → `Err` (not yet implemented through
    ///   this path; the caller should fall back to the interpreter).
    ///
    /// The `extensions` (turboGP query extensions like APPROXIMATE, TIER,
    /// etc.) are passed down to each leaf SELECT so they apply uniformly
    /// across the set operation.
    pub(crate) fn execute_set_query(
        &mut self,
        set: &crate::sql::parser::SetQuery,
        extensions: &crate::sql::extensions::QueryExtensions,
        start: &Instant,
        txn_id: Option<u64>,
    ) -> Result<QueryResult> {
        use crate::sql::parser::SetQuery;
        let _ = txn_id; // currently unused; reserved for future MVCC propagation.
        match set {
            SetQuery::Select(q) => self.execute_select_query(q, extensions, start),
            SetQuery::UnionAll(left, right) => {
                let left_result = self.execute_set_query(left, extensions, start, txn_id)?;
                let right_result = self.execute_set_query(right, extensions, start, txn_id)?;
                Ok(concatenate_results(left_result, right_result, start))
            }
            SetQuery::Union(left, right) => {
                let left_result = self.execute_set_query(left, extensions, start, txn_id)?;
                let right_result = self.execute_set_query(right, extensions, start, txn_id)?;
                let combined = concatenate_results(left_result, right_result, start);
                Ok(deduplicate_rows(combined))
            }
            _ => Err(Error::Other(
                "INTERSECT/EXCEPT set operations are not yet supported via the formal parser".into(),
            )),
        }
    }

    /// Execute a single parsed `SelectQuery` (the leaf of a `SetQuery` tree).
    ///
    /// Mirrors the relevant portion of `execute_inner`'s SELECT path: it
    /// computes the MVCC visibility filter, calls `execute_select` with
    /// the parsed query and extensions, and applies window functions and
    /// DISTINCT post-processing. It does NOT run the indexed-lookup fast
    /// path or the interpreter fallback (those are the responsibility of
    /// `execute_inner` for top-level SELECTs; for set-operation leaves
    /// we go straight to `execute_select`).
    pub(crate) fn execute_select_query(
        &mut self,
        query: &crate::sql::parser::SelectQuery,
        extensions: &crate::sql::extensions::QueryExtensions,
        start: &Instant,
    ) -> Result<QueryResult> {
        use crate::engine::executor::execute_select;
        let mvcc_for_select = if self.mvcc_enabled {
            Some(&self.mvcc_txn_manager)
        } else {
            None
        };
        let mut result = execute_select(
            query,
            None,
            Some(&self.plan_cache),
            extensions,
            &self.catalog,
            &self.kernel_table,
            &self.cost_model,
            mvcc_for_select,
        )?;
        // Apply window functions if any SelectItem::Window is present.
        if query
            .select
            .iter()
            .any(|s| matches!(s, crate::sql::parser::SelectItem::Window { .. }))
        {
            result = apply_window_functions(&result, query);
        }
        // Apply DISTINCT deduplication if requested.
        if query.distinct {
            result = deduplicate_rows(result);
        }
        result.elapsed_us = start.elapsed().as_micros() as u64;
        Ok(result)
    }
}

impl QueryEngine {
    /// Rewrite a SQL string containing `JSON_VALUE(col, 'path')` /
    /// `JSON_QUERY(col, 'path')` calls so that the bare column name is
    /// selected, then post-process the result to apply the JSON
    /// extraction to the string values of that column. Used by
    /// `execute_inner` when the SQL contains a JSON_VALUE/JSON_QUERY call.
    pub(crate) fn execute_with_json_value(
        &mut self,
        sql: &str,
        start: &Instant,
        txn_id: Option<u64>,
    ) -> Result<QueryResult> {
        let calls = extract_json_value_calls(sql);
        if calls.is_empty() {
            // Shouldn't happen — contains_json_value_call returned true — but
            // fall through to the normal path just in case.
            return self.execute_inner(sql, start, txn_id);
        }
        // Rewrite the SQL: replace each call with the bare column name.
        let mut rewritten = String::with_capacity(sql.len());
        let mut last_end = 0;
        for c in &calls {
            rewritten.push_str(&sql[last_end..c.start]);
            rewritten.push_str(&c.col_name);
            last_end = c.end;
        }
        rewritten.push_str(&sql[last_end..]);
        // Execute the rewritten SQL. The rewritten SQL has no JSON_VALUE(...)
        // calls, so this won't re-enter execute_with_json_value.
        let mut result = self.execute_inner(&rewritten, start, txn_id)?;
        // Post-process: for each call, find the result column at the call's
        // SELECT position and apply json_value() / json_query() to its string
        // values.
        for c in &calls {
            let col_idx = c.select_position;
            if col_idx >= result.columns.len() {
                continue;
            }
            // Get the string values from the column. If string_values is
            // None, we can't extract JSON — skip this call.
            let strings = result.columns[col_idx].string_values.clone().unwrap_or_default();
            if strings.is_empty() {
                continue;
            }
            let extracted: Vec<String> = strings
                .iter()
                .map(|s| {
                    if c.is_query {
                        crate::exec::json::json_query(s, &c.path).unwrap_or_default()
                    } else {
                        crate::exec::json::json_value(s, &c.path).unwrap_or_default()
                    }
                })
                .collect();
            // Replace the column with a new one carrying the extracted strings.
            use xxhash_rust::xxh3;
            let values: Vec<u64> = extracted.iter().map(|s| xxh3::xxh3_64(s.as_bytes())).collect();
            let final_name = c.alias.clone().unwrap_or_else(|| {
                if c.is_query {
                    "json_query".into()
                } else {
                    "json_value".into()
                }
            });
            result.columns[col_idx] = ResultColumn {
                name: final_name,
                values,
                string_values: Some(extracted),
                type_oid: 25, // text OID
                null_mask: None,
            };
        }
        result.elapsed_us = start.elapsed().as_micros() as u64;
        Ok(result)
    }
}

// -----------------------------------------------------------------------
// Wave 7 (Task 7.1) — formal UNION ALL dispatch tests.
// -----------------------------------------------------------------------

#[cfg(test)]
mod wave7_union_tests {
    use super::*;

    /// `try_parse_as_set_query` recognises `SELECT ... UNION ALL SELECT ...`
    /// as a `SetQuery::UnionAll` and returns the parsed AST.
    #[test]
    fn try_parse_as_set_query_detects_union_all() {
        let parsed = try_parse_as_set_query("SELECT * FROM t1 UNION ALL SELECT * FROM t2");
        assert!(parsed.is_some(), "UNION ALL should be detected");
        let (set, _ext) = parsed.expect("parsed");
        assert!(
            matches!(set, crate::sql::parser::SetQuery::UnionAll(_, _)),
            "expected UnionAll, got {set:?}"
        );
    }

    /// `try_parse_as_set_query` recognises `UNION` (without `ALL`) as a
    /// `SetQuery::Union`.
    #[test]
    fn try_parse_as_set_query_detects_union() {
        let parsed = try_parse_as_set_query("SELECT id FROM t1 UNION SELECT id FROM t2");
        assert!(parsed.is_some(), "UNION should be detected");
        let (set, _ext) = parsed.expect("parsed");
        assert!(
            matches!(set, crate::sql::parser::SetQuery::Union(_, _)),
            "expected Union, got {set:?}"
        );
    }

    /// A plain `SELECT` (no set operation) returns `None`, so the caller
    /// falls through to the normal SELECT path.
    #[test]
    fn try_parse_as_set_query_returns_none_for_plain_select() {
        assert!(try_parse_as_set_query("SELECT * FROM t").is_none());
        assert!(try_parse_as_set_query("SELECT COUNT(*) FROM t WHERE x = 1").is_none());
    }

    /// A nested UNION ALL (`a UNION ALL b UNION ALL c`) parses as a left-
    /// associative chain of `UnionAll` nodes.
    #[test]
    fn try_parse_as_set_query_handles_nested_union_all() {
        let parsed = try_parse_as_set_query(
            "SELECT 1 FROM t1 UNION ALL SELECT 2 FROM t2 UNION ALL SELECT 3 FROM t3",
        );
        let (set, _) = parsed.expect("parsed");
        // The outer UnionAll should have an inner UnionAll on the left.
        match set {
            crate::sql::parser::SetQuery::UnionAll(left, _right) => {
                assert!(
                    matches!(*left, crate::sql::parser::SetQuery::UnionAll(_, _)),
                    "expected nested UnionAll on the left, got {left:?}"
                );
            }
            other => panic!("expected outer UnionAll, got {other:?}"),
        }
    }

    /// End-to-end: `engine.execute("SELECT * FROM t1 UNION ALL SELECT * FROM t2")`
    /// returns the concatenated rows via the formal AST path.
    #[test]
    fn union_all_uses_formal_ast() {
        let mut engine = QueryEngine::in_memory();
        engine.execute("CREATE TABLE t1 (id INT)").unwrap();
        engine.execute("CREATE TABLE t2 (id INT)").unwrap();
        engine.execute("INSERT INTO t1 VALUES (1), (2)").unwrap();
        engine.execute("INSERT INTO t2 VALUES (3), (4)").unwrap();

        let r = engine
            .execute("SELECT * FROM t1 UNION ALL SELECT * FROM t2")
            .expect("UNION ALL should execute");
        assert_eq!(r.row_count, 4, "expected 4 concatenated rows, got {}", r.row_count);
        // Verify the values are present (order is preserved: t1 rows first).
        let ids: Vec<u64> = r.columns[0].values.iter().copied().collect();
        assert!(ids.contains(&1), "ids = {ids:?}");
        assert!(ids.contains(&2), "ids = {ids:?}");
        assert!(ids.contains(&3), "ids = {ids:?}");
        assert!(ids.contains(&4), "ids = {ids:?}");
    }

    /// End-to-end: `UNION` (without `ALL`) deduplicates.
    #[test]
    fn union_uses_formal_ast_dedup() {
        let mut engine = QueryEngine::in_memory();
        engine.execute("CREATE TABLE t1 (id INT)").unwrap();
        engine.execute("CREATE TABLE t2 (id INT)").unwrap();
        // Both tables have id=1; UNION (not UNION ALL) should dedup.
        engine.execute("INSERT INTO t1 VALUES (1)").unwrap();
        engine.execute("INSERT INTO t2 VALUES (1)").unwrap();

        let r = engine
            .execute("SELECT * FROM t1 UNION SELECT * FROM t2")
            .expect("UNION should execute");
        assert_eq!(r.row_count, 1, "UNION should dedup, got {}", r.row_count);
    }
}

// -----------------------------------------------------------------------
// Wave 7 (Task 7.2) — formal MERGE dispatch tests.
// -----------------------------------------------------------------------

#[cfg(test)]
mod wave7_merge_tests {
    use super::*;

    /// `try_parse_merge_stmt` recognises a complete MERGE statement and
    /// returns the parsed `MergeStmt` AST.
    #[test]
    fn try_parse_merge_stmt_detects_merge() {
        let sql = "MERGE INTO target USING (VALUES (1, 99), (2, 42)) AS source (id, v) \
                   ON target.id = source.id \
                   WHEN MATCHED THEN UPDATE SET v = source.v \
                   WHEN NOT MATCHED THEN INSERT (id, v) VALUES (source.id, source.v)";
        let parsed = try_parse_merge_stmt(sql);
        assert!(parsed.is_some(), "MERGE should be detected");
        let stmt = parsed.expect("parsed");
        assert_eq!(stmt.target, "target");
        // Source is Values with 2 rows and 2 cols.
        match &stmt.source {
            crate::sql::parser::MergeSource::Values { rows, col_names } => {
                assert_eq!(rows.len(), 2, "rows: {rows:?}");
                assert_eq!(col_names, &["id".to_string(), "v".to_string()]);
            }
            other => panic!("expected Values source, got {other:?}"),
        }
        // ON clause: target_col = "id", source_col = "id".
        assert_eq!(stmt.on.target_col, "id");
        assert_eq!(stmt.on.source_col, "id");
        // WHEN MATCHED has 1 Update action.
        assert_eq!(stmt.when_matched.len(), 1);
        // WHEN NOT MATCHED has 1 Insert action.
        assert_eq!(stmt.when_not_matched.len(), 1);
    }

    /// A non-MERGE statement returns `None`, so the caller falls through.
    #[test]
    fn try_parse_merge_stmt_returns_none_for_select() {
        assert!(try_parse_merge_stmt("SELECT * FROM t").is_none());
        assert!(try_parse_merge_stmt("INSERT INTO t VALUES (1)").is_none());
        assert!(try_parse_merge_stmt("CREATE TABLE t (id INT)").is_none());
    }

    /// A malformed MERGE statement returns `None` (parse failure is
    /// converted to None by `try_parse_merge_stmt`).
    #[test]
    fn try_parse_merge_stmt_returns_none_for_malformed() {
        // Missing USING clause.
        assert!(try_parse_merge_stmt("MERGE INTO target ON target.id = source.id").is_none());
        // Missing ON clause.
        assert!(
            try_parse_merge_stmt("MERGE INTO target USING (VALUES (1, 2)) AS s (a, b)").is_none()
        );
    }

    /// `merge_stmt_to_merge` produces the same shape as the old
    /// `parse_merge` hack: source_rows are (join_key, full_row) tuples,
    /// and join_target_col / join_source_col come from the ON clause.
    #[test]
    fn merge_stmt_to_merge_produces_correct_shape() {
        let sql = "MERGE INTO target USING (VALUES (1, 99), (2, 42)) AS source (id, v) \
                   ON target.id = source.id \
                   WHEN MATCHED THEN UPDATE SET v = source.v \
                   WHEN NOT MATCHED THEN INSERT (id, v) VALUES (source.id, source.v)";
        let stmt = try_parse_merge_stmt(sql).expect("parsed");
        let merge = merge_stmt_to_merge(&stmt);
        assert_eq!(merge.target, "target");
        assert_eq!(merge.join_target_col, "id");
        assert_eq!(merge.join_source_col, "id");
        assert_eq!(merge.source_col_names, vec!["id".to_string(), "v".to_string()]);
        // source_rows: [(key="1", ["1", "99"]), (key="2", ["2", "42"])]
        assert_eq!(merge.source_rows.len(), 2);
        assert_eq!(merge.source_rows[0].0, "1");
        assert_eq!(merge.source_rows[0].1, vec!["1".to_string(), "99".to_string()]);
        assert_eq!(merge.source_rows[1].0, "2");
        assert_eq!(merge.source_rows[1].1, vec!["2".to_string(), "42".to_string()]);
        // when_matched: Update([("v", "source.v")]).
        match &merge.when_matched {
            Some(crate::exec::merge::MergeAction::Update(sets)) => {
                assert_eq!(sets, &vec![("v".to_string(), "source.v".to_string())]);
            }
            other => panic!("expected Update, got {other:?}"),
        }
        // when_not_matched_by_target: Insert(["id", "v"], ["source.id", "source.v"]).
        match &merge.when_not_matched_by_target {
            Some(crate::exec::merge::MergeAction::Insert(cols, vals)) => {
                assert_eq!(cols, &vec!["id".to_string(), "v".to_string()]);
                assert_eq!(vals, &vec!["source.id".to_string(), "source.v".to_string()]);
            }
            other => panic!("expected Insert, got {other:?}"),
        }
    }

    /// End-to-end: `engine.execute(merge_sql)` executes via the formal
    /// `MergeStmt` AST path and produces the expected target table state.
    #[test]
    fn merge_uses_formal_ast() {
        let mut engine = QueryEngine::in_memory();
        engine.execute("CREATE TABLE target (id INT, v INT)").unwrap();
        engine.execute("INSERT INTO target VALUES (1, 10)").unwrap();

        let merge_sql = "MERGE INTO target USING (VALUES (1, 99), (2, 42)) AS source (id, v) \
                         ON target.id = source.id \
                         WHEN MATCHED THEN UPDATE SET v = source.v \
                         WHEN NOT MATCHED THEN INSERT (id, v) VALUES (source.id, source.v)";
        let r = engine.execute(merge_sql);
        assert!(r.is_ok(), "MERGE should execute: {:?}", r.err());

        // After MERGE: target should have 2 rows — (1, 99) updated, (2, 42) inserted.
        let r = engine.execute("SELECT COUNT(*) FROM target").unwrap();
        assert_eq!(r.columns[0].values[0], 2, "target should have 2 rows after MERGE");
    }
}
