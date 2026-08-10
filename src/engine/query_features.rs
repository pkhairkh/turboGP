//! Query features — EXPLAIN plan tree, materialized views, plan cache,
//! window function frame support.
//!
//! This module implements the Wave 7 query features:
//!
//! - **EXPLAIN plan tree**: prints the `LogicalPlan` tree (like DuckDB)
//! - **Materialized views**: `CREATE MATERIALIZED VIEW`, `REFRESH`, `DROP`
//! - **Plan cache**: caches `LogicalPlan` by SQL hash for prepared statements
//! - **Window frames**: `ROWS/RANGE BETWEEN N PRECEDING AND M FOLLOWING`

use crate::catalog::Catalog;
use crate::engine::result::{QueryResult, ResultColumn};
use crate::error::{Error, Result};
use crate::planner::{build_plan, CascadesOptimizer, PlanNode};
use crate::sql::lexer::tokenize;
use crate::sql::parser;
use std::collections::HashMap;
use std::sync::Mutex;

// =========================================================================
// EXPLAIN plan tree
// =========================================================================

/// Execute EXPLAIN by building a logical plan and printing it as a tree.
///
/// Returns a single-column text result with the plan tree.
pub fn execute_explain_plan(sql: &str, catalog: &Catalog) -> Result<QueryResult> {
    let tokens = tokenize(sql).map_err(Error::Parse)?;
    let query = parser::parse(tokens).map_err(Error::Parse)?;

    let plan = build_plan(&query)?;
    let optimizer = CascadesOptimizer::new();
    let optimized = optimizer.optimize(plan);

    let plan_text = format!("{}", optimized);

    let mut result = QueryResult::empty();
    result.row_count = 1;
    result.columns = vec![ResultColumn {
        name: "QUERY PLAN".to_string(),
        values: vec![xxhash_rust::xxh3::xxh3_64(plan_text.as_bytes())],
        string_values: Some(vec![plan_text]),
        type_oid: 25, // text OID
        null_mask: None,
    }];
    Ok(result)
}

// =========================================================================
// Materialized views
// =========================================================================

/// A materialized view: stores the result of a SELECT query.
#[derive(Debug, Clone)]
pub struct MaterializedView {
    /// The view name.
    pub name: String,
    /// The SQL query that defines the view.
    pub sql: String,
    /// The materialized result (cached).
    pub result: Option<QueryResult>,
    /// When the view was last refreshed (epoch microseconds).
    pub last_refreshed_us: u64,
}

/// Registry of materialized views.
pub struct MatViewRegistry {
    views: HashMap<String, MaterializedView>,
}

impl MatViewRegistry {
    /// Create a new empty registry.
    pub fn new() -> Self {
        Self { views: HashMap::new() }
    }

    /// Create a materialized view.
    ///
    /// `CREATE MATERIALIZED VIEW name AS SELECT ...`
    pub fn create(&mut self, name: &str, sql: &str) -> Result<()> {
        if self.views.contains_key(name) {
            return Err(Error::Other(format!("materialized view '{}' already exists", name)));
        }
        self.views.insert(name.to_string(), MaterializedView {
            name: name.to_string(),
            sql: sql.to_string(),
            result: None,
            last_refreshed_us: 0,
        });
        Ok(())
    }

    /// Refresh a materialized view by re-executing its query.
    ///
    /// `REFRESH MATERIALIZED VIEW name`
    pub fn refresh(&mut self, name: &str, catalog: &Catalog) -> Result<()> {
        let view = self.views.get_mut(name)
            .ok_or_else(|| Error::Other(format!("materialized view '{}' not found", name)))?;

        // Parse and execute the view's query
        let tokens = tokenize(&view.sql).map_err(Error::Parse)?;
        let query = parser::parse(tokens).map_err(Error::Parse)?;

        // Execute the query against the catalog
        // For now, we use a simple execution path
        let result = execute_query(&query, catalog)?;
        view.result = Some(result);
        view.last_refreshed_us = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_micros() as u64)
            .unwrap_or(0);
        Ok(())
    }

    /// Drop a materialized view.
    ///
    /// `DROP MATERIALIZED VIEW name`
    pub fn drop(&mut self, name: &str) -> Result<()> {
        self.views.remove(name)
            .ok_or_else(|| Error::Other(format!("materialized view '{}' not found", name)))?;
        Ok(())
    }

    /// Get a materialized view's result (if refreshed).
    pub fn get(&self, name: &str) -> Option<&QueryResult> {
        self.views.get(name).and_then(|v| v.result.as_ref())
    }

    /// List all materialized view names.
    pub fn list(&self) -> Vec<String> {
        self.views.keys().cloned().collect()
    }
}

impl Default for MatViewRegistry {
    fn default() -> Self {
        Self::new()
    }
}

/// Parse a CREATE MATERIALIZED VIEW statement.
///
/// Returns (view_name, select_sql) if the statement is a CREATE MATVIEW.
pub fn parse_create_matview(sql: &str) -> Option<(String, String)> {
    let upper = sql.to_uppercase().trim().to_string();
    if !upper.starts_with("CREATE MATERIALIZED VIEW") {
        return None;
    }
    let rest = sql.trim()["CREATE MATERIALIZED VIEW".len()..].trim();
    let as_pos = rest.to_uppercase().find(" AS ")?;
    let name = rest[..as_pos].trim().to_string();
    let select_sql = rest[as_pos + 4..].trim().to_string();
    if name.is_empty() || select_sql.is_empty() {
        return None;
    }
    Some((name, select_sql))
}

/// Parse a REFRESH MATERIALIZED VIEW statement.
///
/// Returns the view name if the statement is a REFRESH MATVIEW.
pub fn parse_refresh_matview(sql: &str) -> Option<String> {
    let upper = sql.to_uppercase().trim().to_string();
    if !upper.starts_with("REFRESH MATERIALIZED VIEW") {
        return None;
    }
    let name = sql.trim()["REFRESH MATERIALIZED VIEW".len()..].trim().to_string();
    if name.is_empty() {
        return None;
    }
    Some(name)
}

/// Parse a DROP MATERIALIZED VIEW statement.
///
/// Returns the view name if the statement is a DROP MATVIEW.
pub fn parse_drop_matview(sql: &str) -> Option<String> {
    let upper = sql.to_uppercase().trim().to_string();
    if !upper.starts_with("DROP MATERIALIZED VIEW") {
        return None;
    }
    let name = sql.trim()["DROP MATERIALIZED VIEW".len()..].trim().to_string();
    if name.is_empty() {
        return None;
    }
    Some(name)
}

// =========================================================================
// Plan cache (prepared statements)
// =========================================================================

/// Plan cache: caches LogicalPlan by SQL hash to skip re-planning.
pub struct PlanCache {
    plans: Mutex<HashMap<u64, PlanNode>>,
}

impl PlanCache {
    /// Create a new empty plan cache.
    pub fn new() -> Self {
        Self { plans: Mutex::new(HashMap::new()) }
    }

    /// Get or build a plan for the given SQL.
    ///
    /// If the plan is already cached, returns the cached plan.
    /// Otherwise, builds the plan and caches it.
    pub fn get_or_build(&self, sql: &str) -> Result<PlanNode> {
        let hash = xxhash_rust::xxh3::xxh3_64(sql.as_bytes());

        // Check cache
        if let Ok(plans) = self.plans.lock() {
            if let Some(plan) = plans.get(&hash) {
                return Ok(plan.clone());
            }
        }

        // Build plan
        let tokens = tokenize(sql).map_err(Error::Parse)?;
        let query = parser::parse(tokens).map_err(Error::Parse)?;
        let plan = build_plan(&query)?;
        let optimizer = CascadesOptimizer::new();
        let optimized = optimizer.optimize(plan);

        // Cache it
        if let Ok(mut plans) = self.plans.lock() {
            plans.insert(hash, optimized.clone());
        }

        Ok(optimized)
    }

    /// Clear the plan cache.
    pub fn clear(&self) {
        if let Ok(mut plans) = self.plans.lock() {
            plans.clear();
        }
    }

    /// Get the number of cached plans.
    pub fn len(&self) -> usize {
        self.plans.lock().map(|p| p.len()).unwrap_or(0)
    }

    /// Check if the cache is empty.
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

impl Default for PlanCache {
    fn default() -> Self {
        Self::new()
    }
}

// =========================================================================
// Window function frame support
// =========================================================================

/// Parse a window frame clause: `ROWS BETWEEN N PRECEDING AND M FOLLOWING`.
///
/// Returns `None` if no frame clause is present.
pub fn parse_window_frame(frame_str: &str) -> Option<WindowFrameSpec> {
    let upper = frame_str.to_uppercase();
    let trimmed = upper.trim();

    // Must start with ROWS or RANGE
    let frame_type = if trimmed.starts_with("ROWS") {
        FrameTypeSpec::Rows
    } else if trimmed.starts_with("RANGE") {
        FrameTypeSpec::Range
    } else {
        return None;
    };

    // Must contain BETWEEN ... AND ...
    let between_pos = trimmed.find("BETWEEN")?;
    let after_between = trimmed[between_pos + 7..].trim();
    let and_pos = after_between.find(" AND ")?;
    let start_str = after_between[..and_pos].trim();
    let end_str = after_between[and_pos + 5..].trim();

    let start = parse_frame_bound(start_str)?;
    let end = parse_frame_bound(end_str)?;

    Some(WindowFrameSpec {
        frame_type,
        start,
        end,
    })
}

/// Parse a single frame bound: `UNBOUNDED PRECEDING`, `N PRECEDING`,
/// `CURRENT ROW`, `N FOLLOWING`, `UNBOUNDED FOLLOWING`.
fn parse_frame_bound(s: &str) -> Option<FrameBoundSpec> {
    let s = s.trim().to_uppercase();
    if s == "UNBOUNDED PRECEDING" {
        return Some(FrameBoundSpec::UnboundedPreceding);
    }
    if s == "UNBOUNDED FOLLOWING" {
        return Some(FrameBoundSpec::UnboundedFollowing);
    }
    if s == "CURRENT ROW" {
        return Some(FrameBoundSpec::CurrentRow);
    }
    // N PRECEDING or N FOLLOWING
    let parts: Vec<&str> = s.split_whitespace().collect();
    if parts.len() == 2 {
        let n: u64 = parts[0].parse().ok()?;
        return match parts[1] {
            "PRECEDING" => Some(FrameBoundSpec::Preceding(n)),
            "FOLLOWING" => Some(FrameBoundSpec::Following(n)),
            _ => None,
        };
    }
    None
}

/// Window frame specification (parsed from SQL).
#[derive(Debug, Clone, PartialEq)]
pub struct WindowFrameSpec {
    pub frame_type: FrameTypeSpec,
    pub start: FrameBoundSpec,
    pub end: FrameBoundSpec,
}

/// Frame type: ROWS or RANGE.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FrameTypeSpec {
    Rows,
    Range,
}

/// Frame bound specification.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FrameBoundSpec {
    UnboundedPreceding,
    Preceding(u64),
    CurrentRow,
    Following(u64),
    UnboundedFollowing,
}

impl std::fmt::Display for WindowFrameSpec {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let ft = match self.frame_type {
            FrameTypeSpec::Rows => "ROWS",
            FrameTypeSpec::Range => "RANGE",
        };
        write!(f, "{} BETWEEN {} AND {}", ft, self.start, self.end)
    }
}

impl std::fmt::Display for FrameBoundSpec {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            FrameBoundSpec::UnboundedPreceding => write!(f, "UNBOUNDED PRECEDING"),
            FrameBoundSpec::Preceding(n) => write!(f, "{} PRECEDING", n),
            FrameBoundSpec::CurrentRow => write!(f, "CURRENT ROW"),
            FrameBoundSpec::Following(n) => write!(f, "{} FOLLOWING", n),
            FrameBoundSpec::UnboundedFollowing => write!(f, "UNBOUNDED FOLLOWING"),
        }
    }
}

// =========================================================================
// Helper: execute a query (simple implementation for matview refresh)
// =========================================================================

fn execute_query(query: &parser::SelectQuery, catalog: &Catalog) -> Result<QueryResult> {
    // Simple execution: return the table data for SELECT *
    if let Some(table) = catalog.get(&query.from) {
        let columns = table.column_names.iter().enumerate().map(|(i, name)| {
            ResultColumn {
                name: name.clone(),
                values: table.columns.get(i).map(|c| c.as_ref().clone()).unwrap_or_default(),
                string_values: None,
                type_oid: 0,
                null_mask: None,
            }
        }).collect();
        Ok(QueryResult {
            columns,
            row_count: table.row_count,
            elapsed_us: 0,
        })
    } else {
        Ok(QueryResult::empty())
    }
}

// =========================================================================
// ALTER TABLE helpers
// =========================================================================

/// Parse an ALTER TABLE statement to determine the action.
///
/// Returns (table_name, action) if the statement is ALTER TABLE.
pub fn parse_alter_table_action(sql: &str) -> Option<(String, AlterAction)> {
    let upper = sql.to_uppercase().trim().to_string();
    if !upper.starts_with("ALTER TABLE") {
        return None;
    }
    let rest = sql.trim()["ALTER TABLE".len()..].trim();

    // Find ADD COLUMN, DROP COLUMN, or RENAME COLUMN
    if let Some(pos) = rest.to_uppercase().find(" ADD COLUMN ") {
        let table_name = rest[..pos].trim().to_string();
        let col_def = rest[pos + 12..].trim().to_string();
        return Some((table_name, AlterAction::AddColumn(col_def)));
    }
    if let Some(pos) = rest.to_uppercase().find(" DROP COLUMN ") {
        let table_name = rest[..pos].trim().to_string();
        let col_name = rest[pos + 13..].trim().trim_end_matches(';').to_string();
        return Some((table_name, AlterAction::DropColumn(col_name)));
    }
    if let Some(pos) = rest.to_uppercase().find(" RENAME COLUMN ") {
        let table_name = rest[..pos].trim().to_string();
        let rest2 = rest[pos + 15..].trim();
        // Expected: old_name TO new_name
        if let Some(to_pos) = rest2.to_uppercase().find(" TO ") {
            let old_name = rest2[..to_pos].trim().to_string();
            let new_name = rest2[to_pos + 4..].trim().to_string();
            return Some((table_name, AlterAction::RenameColumn(old_name, new_name)));
        }
    }
    None
}

/// ALTER TABLE action.
#[derive(Debug, Clone, PartialEq)]
pub enum AlterAction {
    AddColumn(String),
    DropColumn(String),
    RenameColumn(String, String),
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_create_matview() {
        let sql = "CREATE MATERIALIZED VIEW my_view AS SELECT * FROM users";
        let (name, select) = parse_create_matview(sql).unwrap();
        assert_eq!(name, "my_view");
        assert!(select.contains("SELECT * FROM users"));
    }

    #[test]
    fn test_parse_refresh_matview() {
        let sql = "REFRESH MATERIALIZED VIEW my_view";
        let name = parse_refresh_matview(sql).unwrap();
        assert_eq!(name, "my_view");
    }

    #[test]
    fn test_parse_drop_matview() {
        let sql = "DROP MATERIALIZED VIEW my_view";
        let name = parse_drop_matview(sql).unwrap();
        assert_eq!(name, "my_view");
    }

    #[test]
    fn test_matview_registry_create_drop() {
        let mut registry = MatViewRegistry::new();
        registry.create("test_view", "SELECT * FROM t").unwrap();
        assert_eq!(registry.list(), vec!["test_view".to_string()]);
        registry.drop("test_view").unwrap();
        assert!(registry.list().is_empty());
    }

    #[test]
    fn test_matview_duplicate_create_fails() {
        let mut registry = MatViewRegistry::new();
        registry.create("v", "SELECT * FROM t").unwrap();
        assert!(registry.create("v", "SELECT * FROM t").is_err());
    }

    #[test]
    fn test_plan_cache() {
        let cache = PlanCache::new();
        assert!(cache.is_empty());

        let plan1 = cache.get_or_build("SELECT * FROM users").unwrap();
        assert_eq!(cache.len(), 1);

        // Same SQL should return cached plan
        let plan2 = cache.get_or_build("SELECT * FROM users").unwrap();
        assert_eq!(plan1, plan2);
        assert_eq!(cache.len(), 1); // still 1, not 2

        // Different SQL should build a new plan
        let _plan3 = cache.get_or_build("SELECT id FROM users WHERE id > 5").unwrap();
        assert_eq!(cache.len(), 2);
    }

    #[test]
    fn test_parse_window_frame_rows() {
        let frame = parse_window_frame("ROWS BETWEEN 2 PRECEDING AND 2 FOLLOWING").unwrap();
        assert_eq!(frame.frame_type, FrameTypeSpec::Rows);
        assert_eq!(frame.start, FrameBoundSpec::Preceding(2));
        assert_eq!(frame.end, FrameBoundSpec::Following(2));
    }

    #[test]
    fn test_parse_window_frame_range() {
        let frame = parse_window_frame("RANGE BETWEEN UNBOUNDED PRECEDING AND CURRENT ROW").unwrap();
        assert_eq!(frame.frame_type, FrameTypeSpec::Range);
        assert_eq!(frame.start, FrameBoundSpec::UnboundedPreceding);
        assert_eq!(frame.end, FrameBoundSpec::CurrentRow);
    }

    #[test]
    fn test_parse_window_frame_unbounded() {
        let frame = parse_window_frame("ROWS BETWEEN UNBOUNDED PRECEDING AND UNBOUNDED FOLLOWING").unwrap();
        assert_eq!(frame.start, FrameBoundSpec::UnboundedPreceding);
        assert_eq!(frame.end, FrameBoundSpec::UnboundedFollowing);
    }

    #[test]
    fn test_parse_window_frame_invalid() {
        assert!(parse_window_frame("INVALID").is_none());
        assert!(parse_window_frame("ROWS BETWEEN").is_none());
    }

    #[test]
    fn test_window_frame_display() {
        let frame = WindowFrameSpec {
            frame_type: FrameTypeSpec::Rows,
            start: FrameBoundSpec::Preceding(2),
            end: FrameBoundSpec::Following(3),
        };
        let s = format!("{}", frame);
        assert!(s.contains("ROWS BETWEEN 2 PRECEDING AND 3 FOLLOWING"));
    }

    #[test]
    fn test_parse_alter_add_column() {
        let (table, action) = parse_alter_table_action(
            "ALTER TABLE users ADD COLUMN email VARCHAR(100)"
        ).unwrap();
        assert_eq!(table, "users");
        match action {
            AlterAction::AddColumn(col) => assert!(col.contains("email")),
            _ => panic!("expected AddColumn"),
        }
    }

    #[test]
    fn test_parse_alter_drop_column() {
        let (table, action) = parse_alter_table_action(
            "ALTER TABLE users DROP COLUMN email"
        ).unwrap();
        assert_eq!(table, "users");
        match action {
            AlterAction::DropColumn(col) => assert_eq!(col, "email"),
            _ => panic!("expected DropColumn"),
        }
    }

    #[test]
    fn test_parse_alter_rename_column() {
        let (table, action) = parse_alter_table_action(
            "ALTER TABLE users RENAME COLUMN email TO mail"
        ).unwrap();
        assert_eq!(table, "users");
        match action {
            AlterAction::RenameColumn(old, new) => {
                assert_eq!(old, "email");
                assert_eq!(new, "mail");
            }
            _ => panic!("expected RenameColumn"),
        }
    }

    #[test]
    fn test_explain_plan() {
        let catalog = Catalog::new();
        let result = execute_explain_plan("SELECT * FROM users", &catalog).unwrap();
        assert_eq!(result.row_count, 1);
        assert!(result.columns[0].string_values.is_some());
        let plan_text = &result.columns[0].string_values.as_ref().unwrap()[0];
        assert!(plan_text.contains("Scan(table=users"));
    }

    #[test]
    fn test_explain_plan_with_where() {
        let catalog = Catalog::new();
        let result = execute_explain_plan(
            "SELECT * FROM users WHERE id > 100", &catalog
        ).unwrap();
        let plan_text = &result.columns[0].string_values.as_ref().unwrap()[0];
        assert!(plan_text.contains("Filter"));
        assert!(plan_text.contains("Scan"));
    }
}
