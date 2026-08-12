//! Formal PIVOT clause parsing (Production Wiring Wave 7).
//!
//! Replaces the previous `parse_pivot_clause` and `strip_pivot_clause`
//! string-scan hacks that lived in `src/engine/helpers.rs`. The parser
//! module now owns PIVOT parsing and produces the formal
//! [`PivotClause`] AST defined in [`crate::sql::ast`].
//!
//! ## Supported syntax
//!
//! ```sql
//! PIVOT (SUM(amount) FOR quarter IN ('Q1', 'Q2', 'Q3'))
//! PIVOT (COUNT(*) FOR quarter IN (1, 2, 3))
//! PIVOT (AVG(price) FOR region IN ('NA', 'EU', 'APAC'))
//! ```
//!
//! The clause may be followed by `AS <alias>` (which is stripped by
//! [`strip_pivot_clause`] before re-execution of the underlying SELECT).

use crate::sql::ast::PivotClause;

/// Parse a PIVOT clause from a SQL string. Returns `None` if no PIVOT
/// clause is present, or if the clause is malformed.
///
/// Supported syntax (case-insensitive):
///   `PIVOT (SUM(amount) FOR quarter IN ('Q1', 'Q2', 'Q3'))`
///   `PIVOT (COUNT(*) FOR quarter IN (1, 2, 3))`
///   `PIVOT (AVG(price) FOR region IN ('NA', 'EU', 'APAC'))`
///
/// The clause may be followed by `AS <alias>` (which is stripped by
/// [`strip_pivot_clause`] before re-execution of the underlying SELECT).
pub fn parse_pivot_clause(sql: &str) -> Option<PivotClause> {
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
    Some(PivotClause {
        agg,
        value_col,
        pivot_col,
        pivot_values,
    })
}

/// Strip the PIVOT clause (and any trailing `AS alias`) from a SQL string,
/// returning the underlying SELECT that should be executed to produce the
/// input rows for the pivot transformation.
pub fn strip_pivot_clause(sql: &str) -> String {
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
        let alias_len = after_as
            .chars()
            .take_while(|c| c.is_alphanumeric() || *c == '_')
            .count();
        let after_alias = &after_as[alias_len..];
        // Build the result: sql[..pivot_pos] + after_alias.
        return format!("{}{}", &sql[..pivot_pos], after_alias);
    }
    // No AS clause — just concatenate.
    format!("{}{}", &sql[..pivot_pos], &sql[end_of_pivot..])
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Task 7.3 DoD: PIVOT clause parsing produces the formal AST.
    #[test]
    fn pivot_clause_parses_sum_amount_for_quarter() {
        let sql = "SELECT * FROM sales PIVOT (SUM(amount) FOR quarter IN ('Q1', 'Q2', 'Q3'))";
        let clause = parse_pivot_clause(sql).expect("PIVOT clause should parse");
        assert_eq!(clause.agg, "SUM");
        assert_eq!(clause.value_col, "amount");
        assert_eq!(clause.pivot_col, "quarter");
        assert_eq!(clause.pivot_values, vec!["Q1", "Q2", "Q3"]);
    }

    /// PIVOT with COUNT(*) parses the value column as `*`.
    #[test]
    fn pivot_clause_parses_count_star() {
        let sql = "SELECT * FROM t PIVOT (COUNT(*) FOR q IN (1, 2, 3))";
        let clause = parse_pivot_clause(sql).expect("PIVOT clause should parse");
        assert_eq!(clause.agg, "COUNT");
        assert_eq!(clause.value_col, "*");
        assert_eq!(clause.pivot_col, "q");
        assert_eq!(clause.pivot_values, vec!["1", "2", "3"]);
    }

    /// PIVOT clause absent → parse returns None.
    #[test]
    fn pivot_clause_returns_none_when_no_pivot() {
        let sql = "SELECT * FROM sales";
        assert!(parse_pivot_clause(sql).is_none());
    }

    /// strip_pivot_clause removes the PIVOT clause and returns the
    /// underlying SELECT.
    #[test]
    fn strip_pivot_clause_removes_pivot() {
        let sql = "SELECT * FROM sales PIVOT (SUM(amount) FOR quarter IN ('Q1', 'Q2'))";
        let stripped = strip_pivot_clause(sql);
        assert_eq!(stripped, "SELECT * FROM sales ");
    }

    /// strip_pivot_clause removes a trailing `AS alias` too.
    #[test]
    fn strip_pivot_clause_removes_as_alias() {
        let sql = "SELECT * FROM sales PIVOT (SUM(amount) FOR quarter IN ('Q1', 'Q2')) AS p";
        let stripped = strip_pivot_clause(sql);
        assert_eq!(stripped, "SELECT * FROM sales ");
    }

    /// Round-trip: parse + strip works on a complex SQL.
    #[test]
    fn pivot_round_trip_complex() {
        let sql = "SELECT region, product, sales FROM monthly_sales PIVOT (AVG(price) FOR region IN ('NA', 'EU')) AS pvt";
        let clause = parse_pivot_clause(sql).expect("parse");
        assert_eq!(clause.agg, "AVG");
        assert_eq!(clause.value_col, "price");
        assert_eq!(clause.pivot_col, "region");
        assert_eq!(clause.pivot_values, vec!["NA", "EU"]);
        let stripped = strip_pivot_clause(sql);
        assert!(stripped.contains("SELECT region, product, sales FROM monthly_sales"));
        assert!(!stripped.to_lowercase().contains("pivot"));
    }
}
