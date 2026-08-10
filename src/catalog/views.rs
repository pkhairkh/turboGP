//! **WIRED INTO SQL EXECUTION (Wave 53)** — this module is reachable through QueryEngine::execute() via the dispatch path in engine/mod.rs.
//! # Views (Wave 12).
//!
//! Implements CREATE VIEW, DROP VIEW, and view expansion (substituting
//! the view's SELECT when the view is referenced in a query).
//! Supports WITH SCHEMABINDING and WITH CHECK OPTION (parsed but
//! check-option enforcement is deferred to DML integration).

use std::collections::HashMap;

/// A stored view definition.
#[derive(Debug, Clone)]
pub struct ViewDef {
    /// View name (may be schema-qualified).
    pub name: String,
    /// The SELECT statement that defines the view.
    pub select_sql: String,
    /// Optional column aliases (for CREATE VIEW name (col1, col2) AS ...).
    pub column_aliases: Option<Vec<String>>,
    /// True if WITH SCHEMABINDING was specified.
    pub schemabinding: bool,
    /// True if WITH CHECK OPTION was specified.
    pub check_option: bool,
}

/// The view registry: maps view names to their definitions.
#[derive(Debug, Clone, Default)]
pub struct ViewRegistry {
    views: HashMap<String, ViewDef>,
}

impl ViewRegistry {
    /// Create an empty registry.
    pub fn new() -> Self {
        Self { views: HashMap::new() }
    }

    /// Register a view. If a view with the same name exists, it's replaced.
    pub fn create(&mut self, view: ViewDef) {
        self.views.insert(view.name.clone(), view);
    }

    /// Drop a view by name. Returns true if it existed.
    pub fn drop(&mut self, name: &str) -> bool {
        self.views.remove(name).is_some()
    }

    /// Look up a view by name.
    pub fn get(&self, name: &str) -> Option<&ViewDef> {
        self.views.get(name)
    }

    /// List all view names.
    pub fn names(&self) -> Vec<&str> {
        self.views.keys().map(|s| s.as_str()).collect()
    }

    /// Check if a name is a registered view.
    pub fn contains(&self, name: &str) -> bool {
        self.views.contains_key(name)
    }

    /// Expand a view reference in a SQL string. If the SQL contains
    /// `FROM view_name`, replace `view_name` with `(view_select_sql)`.
    /// This is a simple text substitution — a proper AST-based expansion
    /// would be more robust but requires parser integration.
    pub fn expand_views(&self, sql: &str) -> String {
        let mut result = sql.to_string();
        for (name, view) in &self.views {
            // Replace "FROM view_name" with "FROM (view_select_sql) AS view_name"
            let pattern = format!("FROM {name}");
            let replacement = format!("FROM ({}) AS {name}", view.select_sql);
            // Case-insensitive replacement.
            let lower_result = result.to_lowercase();
            let lower_pattern = pattern.to_lowercase();
            if let Some(pos) = lower_result.find(&lower_pattern) {
                result =
                    format!("{}{}{}", &result[..pos], replacement, &result[pos + pattern.len()..]);
            }
        }
        result
    }
}

/// Parse a CREATE VIEW statement. Returns None if the string is not a
/// CREATE VIEW.
pub fn parse_create_view(sql: &str) -> Option<Result<ViewDef, String>> {
    let trimmed = sql.trim();
    let upper = trimmed.to_uppercase();
    if !upper.starts_with("CREATE VIEW ") && !upper.starts_with("CREATE OR ALTER VIEW ") {
        return None;
    }
    Some(parse_create_view_inner(sql))
}

fn parse_create_view_inner(sql: &str) -> Result<ViewDef, String> {
    let trimmed = sql.trim();
    // Skip "CREATE " or "CREATE OR ALTER "
    let after_create = if trimmed.to_uppercase().starts_with("CREATE OR ALTER ") {
        &trimmed["CREATE OR ALTER ".len()..]
    } else {
        &trimmed["CREATE ".len()..]
    };
    // Skip "VIEW "
    let after_view = if after_create.to_uppercase().starts_with("VIEW ") {
        &after_create["VIEW ".len()..]
    } else {
        return Err("expected VIEW after CREATE".into());
    };

    // Parse view name (may be schema.view).
    let mut name_end = 0;
    for (i, c) in after_view.char_indices() {
        if c.is_whitespace() || c == '(' || c == 'A' || c == 'a' {
            // Check if it's "AS" keyword.
            if i + 2 <= after_view.len() {
                let next_two = after_view[i..i + 2].to_uppercase();
                if next_two == "AS" {
                    break;
                }
            }
            if c == '(' {
                break;
            }
        }
        name_end = i + c.len_utf8();
    }
    let name = after_view[..name_end].trim().to_string();
    let rest = after_view[name_end..].trim();

    // Optional column aliases: (col1, col2, ...)
    let (rest_after_aliases, column_aliases) = if rest.starts_with('(') {
        let end = rest.find(')').ok_or("missing ) in view column list")?;
        let cols_str = &rest[1..end];
        let cols: Vec<String> = cols_str.split(',').map(|s| s.trim().to_string()).collect();
        (rest[end + 1..].trim(), Some(cols))
    } else {
        (rest, None)
    };

    // Expect AS.
    let upper_rest = rest_after_aliases.to_uppercase();
    if !upper_rest.starts_with("AS ") {
        return Err("expected AS in CREATE VIEW".into());
    }
    let select_sql = rest_after_aliases["AS ".len()..].trim().to_string();

    // Check for WITH SCHEMABINDING / WITH CHECK OPTION at the end.
    let mut schemabinding = false;
    let mut check_option = false;
    let mut clean_select = select_sql.clone();
    if upper_rest.contains("WITH SCHEMABINDING") {
        schemabinding = true;
        // Remove the WITH SCHEMABINDING from the select.
        let idx = clean_select.to_uppercase().find("WITH SCHEMABINDING");
        if let Some(i) = idx {
            clean_select = format!(
                "{}{}",
                &clean_select[..i],
                &clean_select[i + "WITH SCHEMABINDING".len()..]
            );
            clean_select = clean_select.trim().to_string();
        }
    }
    if upper_rest.contains("WITH CHECK OPTION") {
        check_option = true;
        let idx = clean_select.to_uppercase().find("WITH CHECK OPTION");
        if let Some(i) = idx {
            clean_select =
                format!("{}{}", &clean_select[..i], &clean_select[i + "WITH CHECK OPTION".len()..]);
            clean_select = clean_select.trim().to_string();
        }
    }

    Ok(ViewDef { name, select_sql: clean_select, column_aliases, schemabinding, check_option })
}

/// Parse a DROP VIEW statement. Returns None if not a DROP VIEW.
pub fn parse_drop_view(sql: &str) -> Option<Result<(String, bool), String>> {
    let trimmed = sql.trim();
    let upper = trimmed.to_uppercase();
    if !upper.starts_with("DROP VIEW ") {
        return None;
    }
    let rest = &trimmed["DROP VIEW ".len()..];
    // Optional IF EXISTS.
    let (rest, if_exists) = if rest.to_uppercase().starts_with("IF EXISTS ") {
        (&rest["IF EXISTS ".len()..], true)
    } else {
        (rest, false)
    };
    let name = rest.trim().trim_end_matches(';').trim().to_string();
    Some(Ok((name, if_exists)))
}

// -----------------------------------------------------------------------
// Tests
// -----------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_simple_view() {
        let sql = "CREATE VIEW v1 AS SELECT id, name FROM users";
        let view = parse_create_view(sql).unwrap().unwrap();
        assert_eq!(view.name, "v1");
        assert_eq!(view.select_sql, "SELECT id, name FROM users");
        assert!(view.column_aliases.is_none());
        assert!(!view.schemabinding);
        assert!(!view.check_option);
    }

    #[test]
    fn parse_view_with_aliases() {
        let sql = "CREATE VIEW v1 (user_id, full_name) AS SELECT id, name FROM users";
        let view = parse_create_view(sql).unwrap().unwrap();
        assert_eq!(view.name, "v1");
        assert_eq!(view.column_aliases, Some(vec!["user_id".into(), "full_name".into()]));
    }

    #[test]
    fn parse_view_with_schemabinding() {
        let sql = "CREATE VIEW v1 AS SELECT id FROM users WITH SCHEMABINDING";
        let view = parse_create_view(sql).unwrap().unwrap();
        assert!(view.schemabinding);
        assert_eq!(view.select_sql, "SELECT id FROM users");
    }

    #[test]
    fn parse_view_with_check_option() {
        let sql = "CREATE VIEW v1 AS SELECT id FROM users WHERE active = 1 WITH CHECK OPTION";
        let view = parse_create_view(sql).unwrap().unwrap();
        assert!(view.check_option);
        assert!(view.select_sql.contains("WHERE active = 1"));
    }

    #[test]
    fn parse_qualified_view_name() {
        let sql = "CREATE VIEW HR.v1 AS SELECT id FROM employees";
        let view = parse_create_view(sql).unwrap().unwrap();
        assert_eq!(view.name, "HR.v1");
    }

    #[test]
    fn parse_create_or_alter() {
        let sql = "CREATE OR ALTER VIEW v1 AS SELECT id FROM users";
        let view = parse_create_view(sql).unwrap().unwrap();
        assert_eq!(view.name, "v1");
    }

    #[test]
    fn parse_drop_view_basic() {
        let sql = "DROP VIEW v1";
        let (name, if_exists) = parse_drop_view(sql).unwrap().unwrap();
        assert_eq!(name, "v1");
        assert!(!if_exists);
    }

    #[test]
    fn parse_drop_view_if_exists_test() {
        let sql = "DROP VIEW IF EXISTS v1";
        let (name, if_exists) = parse_drop_view(sql).unwrap().unwrap();
        assert_eq!(name, "v1");
        assert!(if_exists);
    }

    #[test]
    fn not_a_view() {
        assert!(parse_create_view("SELECT 1").is_none());
        assert!(parse_create_view("CREATE TABLE t (id INT)").is_none());
        assert!(parse_drop_view("DROP TABLE t").is_none());
    }

    #[test]
    fn registry_create_get_drop() {
        let mut reg = ViewRegistry::new();
        let view = ViewDef {
            name: "v1".into(),
            select_sql: "SELECT id FROM users".into(),
            column_aliases: None,
            schemabinding: false,
            check_option: false,
        };
        reg.create(view);
        assert!(reg.contains("v1"));
        assert_eq!(reg.get("v1").unwrap().select_sql, "SELECT id FROM users");
        assert!(reg.drop("v1"));
        assert!(!reg.contains("v1"));
    }

    #[test]
    fn expand_view_reference() {
        let mut reg = ViewRegistry::new();
        reg.create(ViewDef {
            name: "active_users".into(),
            select_sql: "SELECT id FROM users WHERE active = 1".into(),
            column_aliases: None,
            schemabinding: false,
            check_option: false,
        });
        let sql = "SELECT count(*) FROM active_users";
        let expanded = reg.expand_views(sql);
        assert!(expanded.contains("(SELECT id FROM users WHERE active = 1)"));
    }

    #[test]
    fn expand_multiple_views() {
        let mut reg = ViewRegistry::new();
        reg.create(ViewDef {
            name: "v1".into(),
            select_sql: "SELECT id FROM t1".into(),
            column_aliases: None,
            schemabinding: false,
            check_option: false,
        });
        reg.create(ViewDef {
            name: "v2".into(),
            select_sql: "SELECT id FROM t2".into(),
            column_aliases: None,
            schemabinding: false,
            check_option: false,
        });
        let sql = "SELECT count(*) FROM v1 UNION ALL SELECT count(*) FROM v2";
        let expanded = reg.expand_views(sql);
        assert!(expanded.contains("t1"));
        assert!(expanded.contains("t2"));
    }
}
