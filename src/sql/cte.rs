//! # Recursive CTE parser (Wave 6).
//!
//! Parses `WITH [RECURSIVE] name AS (anchor UNION ALL recursive) SELECT ...`.
//! The CTE definition is stored as raw SQL strings for the anchor and
//! recursive parts; the executor runs the anchor, then iterates the
//! recursive part (with the CTE name bound to the working set) until no
//! new rows are produced or MAXRECURSION is reached.

/// One CTE definition: name + anchor SQL + optional recursive SQL.
#[derive(Debug, Clone)]
pub struct CteDef {
    /// The CTE name (e.g. "EmployeeHierarchy" in `WITH EmployeeHierarchy AS (...)`).
    pub name: String,
    /// The anchor query (the non-recursive seed).
    pub anchor: String,
    /// The recursive query (references the CTE name). None for non-recursive CTEs.
    pub recursive: Option<String>,
}

/// A parsed WITH clause: one or more CTEs plus the outer query.
#[derive(Debug, Clone)]
pub struct WithClause {
    /// The CTE definitions.
    pub ctes: Vec<CteDef>,
    /// The outer query (everything after the CTE definitions).
    pub outer_query: String,
    /// MAXRECURSION hint (default 100, 0 = unlimited).
    pub max_recursion: u32,
}

/// Parse a WITH clause from a SQL string. Returns None if the string
/// doesn't start with WITH.
pub fn parse_with(sql: &str) -> Option<Result<WithClause, String>> {
    let trimmed = sql.trim_start();
    if !trimmed.to_uppercase().starts_with("WITH ") {
        return None;
    }
    Some(parse_with_inner(sql))
}

fn parse_with_inner(sql: &str) -> Result<WithClause, String> {
    // Find the position of the outer SELECT (the one that's not inside
    // parentheses). We scan the string, tracking paren depth, and find
    // the first top-level SELECT after the CTE definitions.
    let upper = sql.to_uppercase();
    let with_pos = upper.find("WITH").ok_or("expected WITH")?;
    let mut pos = with_pos + 4; // skip "WITH"
                                // Skip optional RECURSIVE
    let rest = &sql[pos..].trim_start();
    if rest.to_uppercase().starts_with("RECURSIVE ") {
        pos = sql.len() - rest.len() + "RECURSIVE ".len();
    }

    // Parse CTE definitions: name AS ( ... ), name AS ( ... ), ...
    let mut ctes = Vec::new();
    let mut max_recursion = 100u32;

    loop {
        // Skip whitespace
        let rest = sql[pos..].trim_start();
        pos = sql.len() - rest.len();

        // Parse CTE name
        let name_end = rest
            .find(|c: char| c.is_whitespace() || c == '(' || c == ',')
            .ok_or("expected CTE name")?;
        let name = rest[..name_end].trim().to_string();
        pos += name_end;

        // Skip whitespace, expect AS
        let rest = sql[pos..].trim_start();
        pos = sql.len() - rest.len();
        if !rest.to_uppercase().starts_with("AS") {
            return Err(format!("expected AS after CTE name '{name}'"));
        }
        pos += 2;

        // Skip whitespace, expect (
        let rest = sql[pos..].trim_start();
        pos = sql.len() - rest.len();
        if !rest.starts_with('(') {
            return Err(format!("expected ( after AS in CTE '{name}'"));
        }

        // Find the matching closing paren, tracking depth.
        let paren_start = pos;
        let mut depth = 0i32;
        let bytes = sql.as_bytes();
        let mut end = pos;
        while end < bytes.len() {
            match bytes[end] {
                b'(' => depth += 1,
                b')' => {
                    depth -= 1;
                    if depth == 0 {
                        break;
                    }
                }
                _ => {}
            }
            end += 1;
        }
        if depth != 0 {
            return Err(format!("unterminated ( in CTE '{name}'"));
        }

        // The CTE body is sql[paren_start+1..end]
        let body = &sql[paren_start + 1..end];
        pos = end + 1;

        // Check if the body contains UNION ALL — if so, it's recursive.
        let body_upper = body.to_uppercase();
        let union_pos = find_top_level_union_all(&body_upper);
        let (anchor, recursive) = if let Some(u_pos) = union_pos {
            let anchor = body[..u_pos].trim().to_string();
            let after = body[u_pos..].trim();
            // Skip "UNION ALL"
            let rec = after["UNION ALL".len()..].trim().to_string();
            (anchor, Some(rec))
        } else {
            (body.trim().to_string(), None)
        };

        ctes.push(CteDef { name, anchor, recursive });

        // Check for comma (another CTE) or end of CTE list
        let rest = sql[pos..].trim_start();
        pos = sql.len() - rest.len();
        if rest.starts_with(',') {
            pos += 1;
            continue;
        }
        break;
    }

    // The rest is the outer query, possibly with OPTION (MAXRECURSION n)
    let mut rest = sql[pos..].trim().to_string();

    // Extract OPTION (MAXRECURSION n) if present
    if let Some(opt_pos) = rest.to_uppercase().find("OPTION") {
        let after_opt = &rest[opt_pos..];
        if let Some(mr_pos) = after_opt.to_uppercase().find("MAXRECURSION") {
            // Parse the number after MAXRECURSION
            let after_mr = &after_opt[mr_pos + "MAXRECURSION".len()..].trim_start();
            let num_end =
                after_mr.find(|c: char| !c.is_ascii_digit() && c != ' ').unwrap_or(after_mr.len());
            if num_end > 0 {
                let num_str = after_mr[..num_end].trim();
                if let Ok(n) = num_str.parse::<u32>() {
                    max_recursion = n;
                }
            }
            // Remove the OPTION clause from the outer query
            rest = rest[..opt_pos].trim().to_string();
        }
    }

    Ok(WithClause { ctes, outer_query: rest, max_recursion })
}

/// Find the position of a top-level "UNION ALL" (not inside parentheses).
fn find_top_level_union_all(s: &str) -> Option<usize> {
    let bytes = s.as_bytes();
    let mut depth = 0i32;
    let mut i = 0;
    while i < bytes.len() {
        match bytes[i] {
            b'(' => depth += 1,
            b')' => depth -= 1,
            _ => {
                if depth == 0 && i + 9 <= bytes.len() {
                    let slice = &s[i..i + 9];
                    if slice.eq_ignore_ascii_case("UNION ALL") {
                        return Some(i);
                    }
                }
            }
        }
        i += 1;
    }
    None
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
        assert_eq!(result.ctes[0].anchor, "SELECT 1 AS x");
        assert!(result.ctes[0].recursive.is_none());
        assert_eq!(result.outer_query, "SELECT * FROM t");
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
        assert_eq!(result.ctes[0].anchor.trim(), "SELECT 10 AS n");
        assert!(result.ctes[0].recursive.as_ref().unwrap().contains("FROM countdown"));
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
        assert!(result.ctes[0].anchor.contains("(SELECT 1 AS x) sub"));
    }
}
