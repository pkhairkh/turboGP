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

/// Parse a MERGE statement (Wave 53 wiring for exec/merge.rs).
///
/// Supports the form:
///   MERGE INTO target [AS t]
///   USING (VALUES (1, 'a'), (2, 'b')) AS s (id, val)
///   ON t.id = s.id
///   WHEN MATCHED THEN UPDATE SET col = val [, ...]
///   WHEN NOT MATCHED THEN INSERT (cols) VALUES (vals)
///
/// Wave 56a fix: the previous implementation hardcoded `source_rows: Vec::new()`,
/// `join_target_col: String::new()`, `join_source_col: String::new()` — so
/// `execute_merge` could never match any target row and the WHEN MATCHED
/// branch was dead. We now parse the USING (VALUES ...) clause to populate
/// `source_rows`, and parse the ON clause to populate the join columns.
///
/// Returns None if the SQL is not a MERGE statement.
pub(crate) fn parse_merge(sql: &str) -> Option<crate::exec::merge::Merge> {
    use crate::exec::merge::{Merge, MergeAction};
    let trimmed = sql.trim();
    let upper = trimmed.to_uppercase();
    if !upper.starts_with("MERGE ") && !upper.starts_with("MERGE INTO ") {
        return None;
    }

    let after_merge = if upper.starts_with("MERGE INTO ") {
        &trimmed["MERGE INTO ".len()..]
    } else {
        &trimmed["MERGE ".len()..]
    };

    // Target table name is the first whitespace-delimited token (optionally
    // followed by `AS alias`).
    let target = after_merge.split_whitespace().next()?.to_string();

    let lower = trimmed.to_lowercase();

    // ---- Parse USING (VALUES (...) , (...), ...) AS alias (col1, col2, ...) ----
    // The source rows are the (join_value, [full_row]) tuples extracted from
    // the VALUES list. The merge module's `source_rows` field is shaped as
    // Vec<(join_value_str, full_row_vals)> — the first element of each tuple
    // is the join key (a stringified u64 or quoted string), and the second
    // is the full row (used by the Insert action).
    let mut source_rows: Vec<(String, Vec<String>)> = Vec::new();
    let mut source_col_names: Vec<String> = Vec::new();
    if let Some(using_pos) = lower.find("using ") {
        let after_using = &trimmed[using_pos + "using ".len()..];
        // Skip whitespace.
        let after_using = after_using.trim_start();
        if after_using.starts_with('(') {
            // Find the matching close paren for the USING (...) group.
            // This may contain nested parens for the VALUES list.
            let mut depth = 0i32;
            let mut using_close = None;
            for (i, c) in after_using.char_indices() {
                match c {
                    '(' => depth += 1,
                    ')' => {
                        depth -= 1;
                        if depth == 0 {
                            using_close = Some(i);
                            break;
                        }
                    }
                    _ => {}
                }
            }
            if let Some(close) = using_close {
                let using_inner = &after_using[1..close];
                // using_inner should start with "VALUES" then have (..), (..)
                let using_inner_lower = using_inner.to_lowercase();
                if let Some(v_pos) = using_inner_lower.find("values") {
                    let after_values = &using_inner[v_pos + "values".len()..];
                    // Parse each (...) tuple.
                    source_rows = parse_values_tuples(after_values);
                }
                // After the USING (...) group, look for "AS alias (col1, col2, ...)"
                // to extract the source column names.
                let after_group = after_using[close + 1..].trim_start();
                let after_as = if after_group.to_uppercase().starts_with("AS ") {
                    &after_group["AS ".len()..]
                } else {
                    after_group
                };
                // Skip the alias identifier.
                let after_alias = after_as
                    .split_whitespace()
                    .next()
                    .map(|n| &after_as[n.len()..])
                    .unwrap_or(after_as)
                    .trim_start();
                if after_alias.starts_with('(') {
                    if let Some(close2) = after_alias.find(')') {
                        source_col_names = after_alias[1..close2]
                            .split(',')
                            .map(|s| s.trim().to_string())
                            .collect();
                    }
                }
            }
        }
    }

    // ---- Parse ON target_col = source_col ----
    let mut join_target_col = String::new();
    let mut join_source_col = String::new();
    if let Some(on_pos) = lower.find(" on ") {
        // Limit the ON clause to the next WHEN keyword (so we don't grab
        // any later "on" in a subquery or string literal).
        let after_on = &trimmed[on_pos + " on ".len()..];
        let when_pos = after_on.to_lowercase().find(" when ").unwrap_or(after_on.len());
        let on_clause = after_on[..when_pos].trim();
        // Parse "target.col = source.col" — split on '=' first.
        if let Some(eq_pos) = on_clause.find('=') {
            let lhs = on_clause[..eq_pos].trim();
            let rhs = on_clause[eq_pos + 1..].trim();
            // Both sides should be qualified "alias.col" — take the part after the dot.
            if let Some(dot_pos) = lhs.rfind('.') {
                join_target_col = lhs[dot_pos + 1..].trim().to_string();
            } else {
                join_target_col = lhs.to_string();
            }
            if let Some(dot_pos) = rhs.rfind('.') {
                join_source_col = rhs[dot_pos + 1..].trim().to_string();
            } else {
                join_source_col = rhs.to_string();
            }
        }
    }

    // ---- Look for WHEN MATCHED THEN UPDATE SET col = val, ... ----
    let mut when_matched: Option<MergeAction> = None;
    let mut when_not_matched_by_target: Option<MergeAction> = None;

    if let Some(pos) = lower.find("when matched then update set") {
        let after = &trimmed[pos + "when matched then update set".len()..];
        // The SET clause runs until the next WHEN keyword (or end of string).
        let set_end = after.to_lowercase().find(" when ").unwrap_or(after.len());
        let assigns_str = after[..set_end].trim();
        // Parse `col = val` pairs separated by commas.
        let assigns: Vec<(String, String)> = split_top_level_commas(assigns_str)
            .into_iter()
            .filter_map(|pair| {
                let pair = pair.trim();
                let eq_pos = pair.find('=')?;
                let col_raw = pair[..eq_pos].trim().to_string();
                let val_raw = pair[eq_pos + 1..].trim().to_string();
                // Strip any "alias." prefix from the LHS column (target.col → col).
                // IMPORTANT: do NOT strip the alias from the RHS value —
                // `source.val` must be preserved so execute_merge can
                // recognize it as a column reference and resolve it against
                // the current source row (Wave 56a fix).
                let col = col_raw.rsplit('.').next().unwrap_or(&col_raw).to_string();
                if col.is_empty() || val_raw.is_empty() {
                    None
                } else {
                    Some((col, val_raw))
                }
            })
            .collect();
        if !assigns.is_empty() {
            when_matched = Some(MergeAction::Update(assigns));
        }
    }

    if let Some(pos) = lower.find("when not matched then insert") {
        let after = &trimmed[pos + "when not matched then insert".len()..];
        // The INSERT clause runs until the next WHEN keyword (or end of string).
        let ins_end = after.to_lowercase().find(" when ").unwrap_or(after.len());
        let ins_str = after[..ins_end].trim();
        // Parse `(col1, col2) VALUES (val1, val2)` — best-effort.
        if let Some(open) = ins_str.find('(') {
            if let Some(close) = ins_str.find(')') {
                let cols: Vec<String> =
                    ins_str[open + 1..close].split(',').map(|s| s.trim().to_string()).collect();
                if let Some(vals_pos) = ins_str[close..].to_lowercase().find("values") {
                    let vals_str = &ins_str[close + vals_pos + "values".len()..];
                    if let Some(v_open) = vals_str.find('(') {
                        if let Some(v_close) = vals_str.find(')') {
                            let vals: Vec<String> = vals_str[v_open + 1..v_close]
                                .split(',')
                                .map(|s| s.trim().to_string())
                                .collect();
                            when_not_matched_by_target = Some(MergeAction::Insert(cols, vals));
                        }
                    }
                }
            }
        }
    }

    // If we parsed source column names, find the join source col's index
    // and rewrite source_rows so the first element of each tuple is the
    // value of the join column (stringified). The merge module uses
    // source_rows[i].0 as the join key to match against target.col_values.
    if !source_col_names.is_empty() && !join_source_col.is_empty() {
        if let Some(src_idx) =
            source_col_names.iter().position(|c| c.eq_ignore_ascii_case(&join_source_col))
        {
            // Each source_row tuple's first element becomes the join key.
            // The Vec<String> carries the full row values in source_col_names order.
            source_rows = source_rows
                .into_iter()
                .map(|(_old_key, mut vals)| {
                    let key = vals.get(src_idx).cloned().unwrap_or_default();
                    (key, vals)
                })
                .collect();
        }
    }

    Some(Merge {
        target,
        source_rows,
        source_col_names,
        join_target_col,
        join_source_col,
        when_matched,
        when_not_matched_by_source: None,
        when_not_matched_by_target,
    })
}

/// Split a string on top-level commas (not inside parentheses or quotes).
/// Used by `parse_merge` to split SET assignments like
/// `col1 = source.col1, col2 = 'literal, with comma'`.
pub(crate) fn split_top_level_commas(s: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut depth = 0i32;
    let mut in_str = false;
    let mut cur = String::new();
    for c in s.chars() {
        match c {
            '\'' => {
                in_str = !in_str;
                cur.push(c);
            }
            '(' if !in_str => {
                depth += 1;
                cur.push(c);
            }
            ')' if !in_str => {
                depth -= 1;
                cur.push(c);
            }
            ',' if depth == 0 && !in_str => {
                out.push(cur.clone());
                cur.clear();
            }
            _ => cur.push(c),
        }
    }
    if !cur.trim().is_empty() {
        out.push(cur);
    }
    out
}

/// Parse the body of a SQL VALUES list — e.g. `(1, 'a'), (2, 'b')` — into
/// a Vec of (first_cell_stringified, full_row) tuples. The first cell is
/// later used as the join key (it's overwritten in parse_merge if a join
/// column index is known).
pub(crate) fn parse_values_tuples(s: &str) -> Vec<(String, Vec<String>)> {
    let mut out = Vec::new();
    let mut chars = s.chars().peekable();
    let mut depth = 0i32;
    let mut cur = String::new();
    let mut tuples: Vec<String> = Vec::new();
    let mut in_str = false;
    while let Some(c) = chars.next() {
        match c {
            '\'' => {
                in_str = !in_str;
                cur.push(c);
            }
            '(' if !in_str => {
                depth += 1;
                if depth == 1 {
                    cur.clear();
                } else {
                    cur.push(c);
                }
            }
            ')' if !in_str => {
                depth -= 1;
                if depth == 0 {
                    tuples.push(cur.clone());
                    cur.clear();
                } else {
                    cur.push(c);
                }
            }
            _ if depth >= 1 => cur.push(c),
            _ => {}
        }
    }
    for t in &tuples {
        let vals: Vec<String> =
            split_top_level_commas(t).into_iter().map(|v| v.trim().to_string()).collect();
        if !vals.is_empty() {
            let first = vals[0].clone();
            out.push((first, vals));
        }
    }
    out
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
pub(crate) struct PivotClause {
    pub(crate) agg: String,
    pub(crate) value_col: String,
    pub(crate) pivot_col: String,
    pub(crate) pivot_values: Vec<String>,
}

/// Parse a PIVOT clause from a SQL string. Returns None if no PIVOT clause
/// is present.
///
/// Supported syntax (case-insensitive):
///   PIVOT (SUM(amount) FOR quarter IN ('Q1', 'Q2', 'Q3'))
///   PIVOT (COUNT(*) FOR quarter IN (1, 2, 3))
///   PIVOT (AVG(price) FOR region IN ('NA', 'EU', 'APAC'))
///
/// The clause may be followed by `AS <alias>` (which is stripped by
/// strip_pivot_clause before re-execution of the underlying SELECT).
pub(crate) fn parse_pivot_clause(sql: &str) -> Option<PivotClause> {
    let lower = sql.to_lowercase();
    let pivot_pos = lower.find("pivot ")?;
    // Must be followed by '(' (possibly with whitespace).
    let after_pivot = &sql[pivot_pos + "pivot ".len()..];
    let after_pivot_trimmed = after_pivot.trim_start();
    if !after_pivot_trimmed.starts_with('(') {
        return None;
    }
    // Find the matching close paren for the PIVOT (...) group.
    let mut depth = 0i32;
    let mut close = None;
    for (i, c) in after_pivot_trimmed.char_indices() {
        match c {
            '(' => depth += 1,
            ')' => {
                depth -= 1;
                if depth == 0 {
                    close = Some(i);
                    break;
                }
            }
            _ => {}
        }
    }
    let close = close?;
    let inner = &after_pivot_trimmed[1..close];
    // inner should look like: SUM(amount) FOR quarter IN ('Q1', 'Q2')
    // or: COUNT(*) FOR quarter IN (1, 2, 3)
    let inner_lower = inner.to_lowercase();
    let for_pos = inner_lower.find(" for ")?;
    let agg_part = inner[..for_pos].trim();
    let after_for = &inner[for_pos + " for ".len()..];
    let after_for_lower = after_for.to_lowercase();
    let in_pos = after_for_lower.find(" in ")?;
    let pivot_col = after_for[..in_pos].trim().to_string();
    let after_in = &after_for[in_pos + " in ".len()..].trim_start();
    // after_in should start with '(' and end with ')'.
    if !after_in.starts_with('(') {
        return None;
    }
    let in_close = after_in.find(')')?;
    let values_str = &after_in[1..in_close];
    // Parse the values: split on commas, strip quotes/brackets.
    let pivot_values: Vec<String> = values_str
        .split(',')
        .map(|s| {
            let s = s.trim();
            // Strip single quotes.
            let s = if s.starts_with('\'') && s.ends_with('\'') && s.len() >= 2 {
                &s[1..s.len() - 1]
            } else {
                s
            };
            // Strip square brackets (SQL Server style [Q1]).
            let s = if s.starts_with('[') && s.ends_with(']') && s.len() >= 2 {
                &s[1..s.len() - 1]
            } else {
                s
            };
            s.to_string()
        })
        .filter(|s| !s.is_empty())
        .collect();
    if pivot_values.is_empty() {
        return None;
    }
    // Parse the agg part: AGG_FUNC(arg). The arg may be '*' or a column name.
    let open = agg_part.find('(')?;
    let close_paren = agg_part.rfind(')')?;
    let agg = agg_part[..open].trim().to_uppercase();
    let value_col = agg_part[open + 1..close_paren].trim().to_string();
    if agg.is_empty() || value_col.is_empty() {
        return None;
    }
    Some(PivotClause { agg, value_col, pivot_col, pivot_values })
}

/// Strip the PIVOT clause (and any trailing `AS alias`) from a SQL string,
/// returning the underlying SELECT that should be executed to produce the
/// input rows for the pivot transformation.
pub(crate) fn strip_pivot_clause(sql: &str) -> String {
    let lower = sql.to_lowercase();
    let pivot_pos = match lower.find("pivot ") {
        Some(p) => p,
        None => return sql.to_string(),
    };
    // Walk forward from pivot_pos to find the matching close paren.
    let after_pivot = &sql[pivot_pos + "pivot ".len()..];
    let after_pivot_trimmed = after_pivot.trim_start();
    let paren_offset = after_pivot.len() - after_pivot_trimmed.len();
    let mut depth = 0i32;
    let mut close = None;
    for (i, c) in after_pivot_trimmed.char_indices() {
        match c {
            '(' => depth += 1,
            ')' => {
                depth -= 1;
                if depth == 0 {
                    close = Some(i);
                    break;
                }
            }
            _ => {}
        }
    }
    let close = match close {
        Some(c) => c,
        None => return sql.to_string(),
    };
    // The PIVOT clause spans [pivot_pos, pivot_pos + "pivot ".len() + paren_offset + close + 1).
    let end_of_pivot = pivot_pos + "pivot ".len() + paren_offset + close + 1;
    // After the PIVOT clause, there may be `AS <alias>` — strip that too.
    let rest = &sql[end_of_pivot..];
    let rest_trimmed_start = rest.trim_start();
    if rest_trimmed_start.to_uppercase().starts_with("AS ") {
        let after_as = &rest_trimmed_start["AS ".len()..];
        // Skip the alias identifier (alphanumeric + underscore).
        let alias_len = after_as.chars().take_while(|c| c.is_alphanumeric() || *c == '_').count();
        let after_alias = &after_as[alias_len..];
        // Build the result: sql[..pivot_pos] + after_alias.
        return format!("{}{}", &sql[..pivot_pos], after_alias);
    }
    // No AS clause — just concatenate.
    format!("{}{}", &sql[..pivot_pos], &sql[end_of_pivot..])
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
// Wave 60c: UNION ALL wiring.
// -----------------------------------------------------------------------

/// Split a SQL string at the first top-level `UNION ALL` keyword
/// (case-insensitive). Returns (left_sql, right_sql) if found, else None.
///
/// "Top-level" means the UNION ALL is not inside parentheses (e.g. not in a
/// subquery). This is a simple heuristic — it doesn't handle UNION (without
/// ALL) or INTERSECT/EXCEPT.
pub(crate) fn split_union_all(sql: &str) -> Option<(String, String)> {
    let lower = sql.to_lowercase();
    let mut search_from = 0;
    loop {
        let pos = lower[search_from..].find("union all")?;
        let abs_pos = search_from + pos;
        // Check that this is a top-level UNION ALL (not inside parens).
        let before = &sql[..abs_pos];
        let depth = before.chars().fold(0i32, |acc, c| match c {
            '(' => acc + 1,
            ')' => acc - 1,
            _ => acc,
        });
        if depth == 0 {
            let left = sql[..abs_pos].trim().to_string();
            let right = sql[abs_pos + "union all".len()..].trim().to_string();
            if !left.is_empty() && !right.is_empty() {
                return Some((left, right));
            }
        }
        search_from = abs_pos + "union all".len();
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
