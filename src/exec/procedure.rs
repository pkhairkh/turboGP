//! **WIRED INTO SQL EXECUTION (Wave 53)** — this module is reachable through QueryEngine::execute() via the dispatch path in engine/mod.rs.
//! # Stored procedures, functions, TVPs, SESSION_CONTEXT (Wave 13).
//!
//! Implements:
//! - CREATE PROCEDURE / CREATE FUNCTION with parameter lists
//! - EXEC procedure_name [args]
//! - Table-valued parameters (TVPs): CREATE TYPE ... AS TABLE
//! - SESSION_CONTEXT: per-session key/value map
//! - sp_set_session_context / SESSION_CONTEXT()

use std::collections::HashMap;

/// A parameter definition: name, type, default value.
#[derive(Debug, Clone)]
pub struct ParamDef {
    pub name: String,
    pub type_name: String,
    pub default: Option<String>,
}

/// A stored procedure or function definition.
#[derive(Debug, Clone)]
pub struct ProcedureDef {
    pub name: String,
    pub params: Vec<ParamDef>,
    /// The body: one or more SQL statements separated by semicolons.
    pub body: String,
    /// True if this is a function (returns a value/table), false if a procedure.
    pub is_function: bool,
    /// For functions: the return type name (e.g. "TABLE", "INT", "VARCHAR").
    pub return_type: Option<String>,
}

/// The procedure registry.
#[derive(Debug, Clone, Default)]
pub struct ProcedureRegistry {
    procs: HashMap<String, ProcedureDef>,
}

impl ProcedureRegistry {
    pub fn new() -> Self {
        Self { procs: HashMap::new() }
    }

    pub fn create(&mut self, proc_def: ProcedureDef) {
        self.procs.insert(proc_def.name.clone(), proc_def);
    }

    pub fn drop(&mut self, name: &str) -> bool {
        self.procs.remove(name).is_some()
    }

    pub fn get(&self, name: &str) -> Option<&ProcedureDef> {
        self.procs.get(name)
    }

    pub fn contains(&self, name: &str) -> bool {
        self.procs.contains_key(name)
    }

    pub fn names(&self) -> Vec<&str> {
        self.procs.keys().map(|s| s.as_str()).collect()
    }
}

/// A table-valued parameter type definition.
#[derive(Debug, Clone)]
pub struct TableType {
    pub name: String,
    pub columns: Vec<(String, String)>, // (column_name, type_name)
}

/// The TVP type registry.
#[derive(Debug, Clone, Default)]
pub struct TableTypeRegistry {
    types: HashMap<String, TableType>,
}

impl TableTypeRegistry {
    pub fn new() -> Self {
        Self { types: HashMap::new() }
    }

    pub fn create(&mut self, t: TableType) {
        self.types.insert(t.name.clone(), t);
    }

    pub fn get(&self, name: &str) -> Option<&TableType> {
        self.types.get(name)
    }

    pub fn contains(&self, name: &str) -> bool {
        self.types.contains_key(name)
    }
}

/// Per-session context: a key/value map for SESSION_CONTEXT.
#[derive(Debug, Clone, Default)]
pub struct SessionContext {
    map: HashMap<String, String>,
}

impl SessionContext {
    pub fn new() -> Self {
        Self { map: HashMap::new() }
    }

    /// Set a session context value (sp_set_session_context).
    pub fn set(&mut self, key: impl Into<String>, value: impl Into<String>) {
        self.map.insert(key.into().to_lowercase(), value.into());
    }

    /// Get a session context value (SESSION_CONTEXT(N'key')).
    pub fn get(&self, key: &str) -> Option<&str> {
        self.map.get(&key.to_lowercase()).map(|s| s.as_str())
    }

    /// Remove a key.
    pub fn remove(&mut self, key: &str) -> bool {
        self.map.remove(&key.to_lowercase()).is_some()
    }

    /// List all keys.
    pub fn keys(&self) -> Vec<&str> {
        self.map.keys().map(|s| s.as_str()).collect()
    }
}

/// Parse a CREATE PROCEDURE or CREATE FUNCTION statement.
/// Returns None if not a CREATE PROC/FUNCTION.
pub fn parse_create_procedure(sql: &str) -> Option<Result<ProcedureDef, String>> {
    let upper = sql.trim().to_uppercase();
    if !upper.starts_with("CREATE PROCEDURE ")
        && !upper.starts_with("CREATE OR ALTER PROCEDURE ")
        && !upper.starts_with("CREATE FUNCTION ")
        && !upper.starts_with("CREATE OR ALTER FUNCTION ")
    {
        return None;
    }
    Some(parse_create_procedure_inner(sql))
}

fn parse_create_procedure_inner(sql: &str) -> Result<ProcedureDef, String> {
    let trimmed = sql.trim();
    let after_create = if trimmed.to_uppercase().starts_with("CREATE OR ALTER ") {
        &trimmed["CREATE OR ALTER ".len()..]
    } else {
        &trimmed["CREATE ".len()..]
    };

    let (is_function, after_keyword) = if after_create.to_uppercase().starts_with("PROCEDURE ") {
        (false, &after_create["PROCEDURE ".len()..])
    } else if after_create.to_uppercase().starts_with("FUNCTION ") {
        (true, &after_create["FUNCTION ".len()..])
    } else {
        return Err("expected PROCEDURE or FUNCTION".into());
    };

    // Parse name (may be schema.name). Stop at whitespace or ( or AS.
    let name_end = after_keyword
        .find(|c: char| c.is_whitespace() || c == '(')
        .ok_or("expected procedure name")?;
    let name = after_keyword[..name_end].trim().to_string();
    let rest = after_keyword[name_end..].trim();

    // Parse parameter list.
    let (rest_after_params, params) = if rest.starts_with('(') {
        let end = rest.find(')').ok_or("missing ) in parameter list")?;
        let params_str = &rest[1..end];
        let params = parse_params(params_str)?;
        (rest[end + 1..].trim(), params)
    } else {
        (rest, Vec::new())
    };

    // For functions, parse RETURNS clause.
    let (body, return_type) = if is_function {
        let upper_rest = rest_after_params.to_uppercase();
        if upper_rest.starts_with("RETURNS ") {
            let after_returns = &rest_after_params["RETURNS ".len()..];
            // Return type is up to AS or BEGIN.
            let type_end = after_returns
                .to_uppercase()
                .find(" AS ")
                .or_else(|| after_returns.to_uppercase().find("BEGIN"));
            if let Some(te) = type_end {
                let rtype = after_returns[..te].trim().to_string();
                let body = after_returns[te..].trim().to_string();
                (body, Some(rtype))
            } else {
                (after_returns.to_string(), Some(after_returns.trim().to_string()))
            }
        } else {
            (rest_after_params.to_string(), None)
        }
    } else {
        // Procedure: body starts at AS.
        let upper_rest = rest_after_params.to_uppercase();
        if upper_rest.starts_with("AS ") {
            (rest_after_params["AS ".len()..].trim().to_string(), None)
        } else {
            (rest_after_params.to_string(), None)
        }
    };

    // Clean up the body: strip BEGIN/END wrapper if present.
    let clean_body =
        body.trim().trim_start_matches("BEGIN").trim().trim_end_matches("END").trim().to_string();

    Ok(ProcedureDef { name, params, body: clean_body, is_function, return_type })
}

fn parse_params(s: &str) -> Result<Vec<ParamDef>, String> {
    if s.trim().is_empty() {
        return Ok(Vec::new());
    }
    let mut params = Vec::new();
    for part in s.split(',') {
        let part = part.trim();
        if part.is_empty() {
            continue;
        }
        // Format: @name TYPE [DEFAULT value] or name TYPE [DEFAULT value]
        let tokens: Vec<&str> = part.split_whitespace().collect();
        if tokens.len() < 2 {
            return Err(format!("invalid parameter: {part}"));
        }
        let name = tokens[0].trim_start_matches('@').to_string();
        let type_name = tokens[1].to_uppercase();
        let default = if tokens.len() >= 4 && tokens[2].eq_ignore_ascii_case("DEFAULT") {
            Some(tokens[3].to_string())
        } else {
            None
        };
        params.push(ParamDef { name, type_name, default });
    }
    Ok(params)
}

/// Parse a CREATE TYPE ... AS TABLE statement (for TVPs).
pub fn parse_create_type(sql: &str) -> Option<Result<TableType, String>> {
    let upper = sql.trim().to_uppercase();
    if !upper.starts_with("CREATE TYPE ") {
        return None;
    }
    Some(parse_create_type_inner(sql))
}

fn parse_create_type_inner(sql: &str) -> Result<TableType, String> {
    let after_create = &sql.trim()["CREATE ".len()..];
    let after_type = if after_create.to_uppercase().starts_with("TYPE ") {
        &after_create["TYPE ".len()..]
    } else {
        return Err("expected TYPE after CREATE".into());
    };

    // Parse type name.
    let name_end = after_type
        .find(|c: char| c.is_whitespace() || c == 'A' || c == 'a')
        .ok_or("expected type name")?;
    let name = after_type[..name_end].trim().to_string();
    let rest = after_type[name_end..].trim();

    // Expect "AS TABLE (".
    let upper_rest = rest.to_uppercase();
    if !upper_rest.starts_with("AS TABLE (") {
        return Err("expected AS TABLE ( in CREATE TYPE".into());
    }

    let cols_str = &rest["AS TABLE (".len()..];
    let end = cols_str.rfind(')').ok_or("missing ) in column list")?;
    let cols_str = &cols_str[..end];

    let mut columns = Vec::new();
    for col_def in cols_str.split(',') {
        let tokens: Vec<&str> = col_def.trim().split_whitespace().collect();
        if tokens.len() >= 2 {
            columns.push((tokens[0].to_string(), tokens[1].to_uppercase()));
        }
    }

    Ok(TableType { name, columns })
}

/// Parse an EXEC statement: `EXEC proc_name [arg1, arg2, ...]`.
/// Returns (procedure_name, arguments).
pub fn parse_exec(sql: &str) -> Option<Result<(String, Vec<String>), String>> {
    let upper = sql.trim().to_uppercase();
    if !upper.starts_with("EXEC ") && !upper.starts_with("EXECUTE ") {
        return None;
    }
    Some(parse_exec_inner(sql))
}

fn parse_exec_inner(sql: &str) -> Result<(String, Vec<String>), String> {
    let after_exec = if sql.trim().to_uppercase().starts_with("EXECUTE ") {
        &sql.trim()["EXECUTE ".len()..]
    } else {
        &sql.trim()["EXEC ".len()..]
    };
    let parts: Vec<&str> = after_exec.split_whitespace().collect();
    if parts.is_empty() {
        return Err("expected procedure name after EXEC".into());
    }
    let name = parts[0].trim_start_matches('@').to_string();
    let args: Vec<String> = if parts.len() > 1 {
        parts[1..].iter().map(|s| s.trim_end_matches(',').to_string()).collect()
    } else {
        Vec::new()
    };
    Ok((name, args))
}

// -----------------------------------------------------------------------
// Tests
// -----------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_simple_procedure() {
        let sql = "CREATE PROCEDURE get_count AS SELECT count(*) FROM users";
        let proc = parse_create_procedure(sql).unwrap().unwrap();
        assert_eq!(proc.name, "get_count");
        assert!(proc.params.is_empty());
        assert!(!proc.is_function);
        assert_eq!(proc.body, "SELECT count(*) FROM users");
    }

    #[test]
    fn parse_procedure_with_params() {
        let sql = "CREATE PROCEDURE get_user (@id INT, @name VARCHAR DEFAULT 'unknown') AS SELECT * FROM users WHERE id = @id";
        let proc = parse_create_procedure(sql).unwrap().unwrap();
        assert_eq!(proc.params.len(), 2);
        assert_eq!(proc.params[0].name, "id");
        assert_eq!(proc.params[0].type_name, "INT");
        assert_eq!(proc.params[1].name, "name");
        assert_eq!(proc.params[1].default, Some("'unknown'".into()));
    }

    #[test]
    fn parse_function_with_returns() {
        let sql = "CREATE FUNCTION get_total (@id INT) RETURNS INT AS BEGIN RETURN 42 END";
        let func = parse_create_procedure(sql).unwrap().unwrap();
        assert!(func.is_function);
        assert_eq!(func.return_type, Some("INT".into()));
        assert_eq!(func.params.len(), 1);
    }

    #[test]
    fn parse_inline_table_function() {
        let sql = "CREATE FUNCTION get_sales (@emp_id INT) RETURNS TABLE AS RETURN (SELECT * FROM sales WHERE employee_id = @emp_id)";
        let func = parse_create_procedure(sql).unwrap().unwrap();
        assert!(func.is_function);
        assert_eq!(func.return_type, Some("TABLE".into()));
        assert!(func.body.contains("SELECT"));
    }

    #[test]
    fn parse_create_or_alter_procedure() {
        let sql = "CREATE OR ALTER PROCEDURE my_proc AS SELECT 1";
        let proc = parse_create_procedure(sql).unwrap().unwrap();
        assert_eq!(proc.name, "my_proc");
    }

    #[test]
    fn parse_qualified_proc_name() {
        let sql = "CREATE PROCEDURE Sales.get_total AS SELECT 1";
        let proc = parse_create_procedure(sql).unwrap().unwrap();
        assert_eq!(proc.name, "Sales.get_total");
    }

    #[test]
    fn parse_create_type_table() {
        let sql =
            "CREATE TYPE OrderItemType AS TABLE (ProductID INT, Quantity INT, UnitPrice DECIMAL)";
        let t = parse_create_type(sql).unwrap().unwrap();
        assert_eq!(t.name, "OrderItemType");
        assert_eq!(t.columns.len(), 3);
        assert_eq!(t.columns[0].0, "ProductID");
        assert_eq!(t.columns[0].1, "INT");
        assert_eq!(t.columns[2].1, "DECIMAL");
    }

    #[test]
    fn parse_exec_basic() {
        let sql = "EXEC get_count";
        let (name, args) = parse_exec(sql).unwrap().unwrap();
        assert_eq!(name, "get_count");
        assert!(args.is_empty());
    }

    #[test]
    fn parse_exec_with_args() {
        let sql = "EXEC get_user 42, 'Alice'";
        let (name, args) = parse_exec(sql).unwrap().unwrap();
        assert_eq!(name, "get_user");
        assert_eq!(args.len(), 2);
        assert_eq!(args[0], "42");
        assert_eq!(args[1], "'Alice'");
    }

    #[test]
    fn parse_execute_keyword() {
        let sql = "EXECUTE my_proc";
        let (name, _) = parse_exec(sql).unwrap().unwrap();
        assert_eq!(name, "my_proc");
    }

    #[test]
    fn not_a_procedure() {
        assert!(parse_create_procedure("SELECT 1").is_none());
        assert!(parse_create_procedure("CREATE TABLE t (id INT)").is_none());
    }

    #[test]
    fn procedure_registry_crud() {
        let mut reg = ProcedureRegistry::new();
        reg.create(ProcedureDef {
            name: "my_proc".into(),
            params: vec![],
            body: "SELECT 1".into(),
            is_function: false,
            return_type: None,
        });
        assert!(reg.contains("my_proc"));
        assert!(reg.drop("my_proc"));
        assert!(!reg.contains("my_proc"));
    }

    #[test]
    fn table_type_registry() {
        let mut reg = TableTypeRegistry::new();
        reg.create(TableType {
            name: "OrderType".into(),
            columns: vec![("id".into(), "INT".into())],
        });
        assert!(reg.contains("OrderType"));
        assert_eq!(reg.get("OrderType").unwrap().columns.len(), 1);
    }

    #[test]
    fn session_context_set_get() {
        let mut ctx = SessionContext::new();
        ctx.set("UserID", "42");
        ctx.set("Department", "Engineering");
        assert_eq!(ctx.get("userid"), Some("42"));
        assert_eq!(ctx.get("USERID"), Some("42"));
        assert_eq!(ctx.get("Department"), Some("Engineering"));
        assert_eq!(ctx.get("missing"), None);
    }

    #[test]
    fn session_context_remove() {
        let mut ctx = SessionContext::new();
        ctx.set("key", "value");
        assert!(ctx.remove("key"));
        assert!(!ctx.remove("key"));
    }

    #[test]
    fn session_context_case_insensitive() {
        let mut ctx = SessionContext::new();
        ctx.set("MyKey", "val");
        assert_eq!(ctx.get("mykey"), Some("val"));
        assert_eq!(ctx.get("MYKEY"), Some("val"));
        assert_eq!(ctx.get("MyKey"), Some("val"));
    }
}
