//! **WIRED INTO SQL EXECUTION (Wave 56c)** — this module is reachable
//! through `QueryEngine::execute()` via `execute_with_json_value` in
//! `engine/mod.rs`. The engine detects `JSON_VALUE(col, 'path')` and
//! `JSON_QUERY(col, 'path')` in the SQL string, rewrites each call to
//! `col` (tracking the SELECT-list position), executes the rewritten SQL,
//! and post-processes the result by applying `json_value()` / `json_query()`
//! to each string value in the corresponding column. Wave 56c also fixed
//! `execute_insert` to preserve original strings in the `string_columns`
//! sidecar for VARCHAR / NVARCHAR / TEXT columns (previously strings were
//! hashed to u64 and the original was lost, so JSON_VALUE / LIKE / range
//! comparisons on inserted strings were broken).
//!
//! Supported SQL syntax:
//! ```sql
//!   SELECT JSON_VALUE(payload, '$.name') FROM docs
//!   SELECT JSON_VALUE(payload, '$.name') AS username FROM docs
//!   SELECT JSON_QUERY(payload, '$.user') FROM docs
//! ```
//! # JSON functions (Wave 9).
//!
//! Implements: JSON_VALUE, JSON_QUERY, JSON_MODIFY, ISJSON, FOR JSON PATH.
//! Uses serde_json for parsing and serialization.

use serde_json::{json, Value};

/// Extract a scalar value from a JSON string at the given path.
/// Path uses SQL/JSON path syntax: `$.field.subfield`.
/// Returns the value as a string, or NULL if the path doesn't exist.
pub fn json_value(json_str: &str, path: &str) -> Option<String> {
    let v: Value = serde_json::from_str(json_str).ok()?;
    let val = navigate_path(&v, path)?;
    match val {
        Value::Null => None,
        Value::Bool(b) => Some(b.to_string()),
        Value::Number(n) => Some(n.to_string()),
        Value::String(s) => Some(s),
        // JSON_VALUE returns NULL for objects/arrays (use JSON_QUERY for those).
        Value::Object(_) | Value::Array(_) => None,
    }
}

/// Extract an object or array from a JSON string at the given path.
pub fn json_query(json_str: &str, path: &str) -> Option<String> {
    let v: Value = serde_json::from_str(json_str).ok()?;
    let val = navigate_path(&v, path)?;
    match val {
        Value::Object(_) | Value::Array(_) => Some(val.to_string()),
        _ => None,
    }
}

/// Modify a JSON string at the given path. Supports:
/// - Setting a value: `JSON_MODIFY(json, '$.field', value)`
/// - Appending to an array: `JSON_MODIFY(json, 'append $.array', value)`
/// - Deleting a key: `JSON_MODIFY(json, '$.field', NULL)`
pub fn json_modify(json_str: &str, path: &str, value: &str) -> String {
    let mut v: Value = serde_json::from_str(json_str).unwrap_or(json!({}));

    // Check for "append" prefix.
    let (is_append, clean_path) =
        if path.to_lowercase().starts_with("append ") { (true, &path[7..]) } else { (false, path) };

    // Parse the value: try as JSON, then as string, then as number/bool/null.
    let new_val: Value = if value.eq_ignore_ascii_case("null") {
        Value::Null
    } else if value.starts_with('\'') && value.ends_with('\'') {
        Value::String(value[1..value.len() - 1].to_string())
    } else if value.starts_with('"') && value.ends_with('"') {
        serde_json::from_str(value).unwrap_or(Value::String(value.to_string()))
    } else if value == "true" || value == "false" {
        Value::Bool(value.parse().unwrap_or(false))
    } else if let Ok(n) = value.parse::<i64>() {
        json!(n)
    } else if let Ok(f) = value.parse::<f64>() {
        json!(f)
    } else {
        Value::String(value.to_string())
    };

    if is_append {
        append_to_path(&mut v, clean_path, new_val);
    } else if new_val.is_null() {
        delete_path(&mut v, clean_path);
    } else {
        set_path(&mut v, clean_path, new_val);
    }

    v.to_string()
}

/// Check if a string is valid JSON.
pub fn is_json(s: &str) -> bool {
    serde_json::from_str::<Value>(s).is_ok()
}

/// Check if a JSON path exists in a JSON string.
pub fn json_path_exists(json_str: &str, path: &str) -> bool {
    let v: Value = match serde_json::from_str(json_str) {
        Ok(v) => v,
        Err(_) => return false,
    };
    navigate_path(&v, path).is_some()
}

/// OPENJSON: parse a JSON array and return rows. Each element becomes a row
/// with columns: key (index), value, type.
pub fn openjson_default(json_str: &str) -> Vec<(String, String, String)> {
    let v: Value = match serde_json::from_str(json_str) {
        Ok(v) => v,
        Err(_) => return Vec::new(),
    };
    match v {
        Value::Array(arr) => arr
            .iter()
            .enumerate()
            .map(|(i, v)| {
                let val_str = match v {
                    Value::Object(_) | Value::Array(_) => v.to_string(),
                    _ => v.to_string(),
                };
                let type_str = match v {
                    Value::Null => "null".into(),
                    Value::Bool(_) => "boolean".into(),
                    Value::Number(n) if n.is_i64() => "int".into(),
                    Value::Number(n) if n.is_u64() => "uint".into(),
                    Value::Number(_) => "float".into(),
                    Value::String(_) => "string".into(),
                    Value::Array(_) => "array".into(),
                    Value::Object(_) => "object".into(),
                };
                (i.to_string(), val_str, type_str)
            })
            .collect(),
        Value::Object(obj) => obj
            .iter()
            .map(|(k, v)| {
                let val_str = match v {
                    Value::Object(_) | Value::Array(_) => v.to_string(),
                    _ => v.to_string(),
                };
                let type_str = match v {
                    Value::Null => "null".into(),
                    Value::Bool(_) => "boolean".into(),
                    Value::Number(n) if n.is_i64() => "int".into(),
                    Value::Number(n) if n.is_u64() => "uint".into(),
                    Value::Number(_) => "float".into(),
                    Value::String(_) => "string".into(),
                    Value::Array(_) => "array".into(),
                    Value::Object(_) => "object".into(),
                };
                (k.clone(), val_str, type_str)
            })
            .collect(),
        _ => Vec::new(),
    }
}

/// OPENJSON with explicit schema: parse a JSON array and extract fields
/// according to the provided (column_name, path) pairs.
pub fn openjson_with_schema(json_str: &str, schema: &[(String, String)]) -> Vec<Vec<String>> {
    let v: Value = match serde_json::from_str(json_str) {
        Ok(v) => v,
        Err(_) => return Vec::new(),
    };
    match v {
        Value::Array(arr) => arr
            .iter()
            .map(|elem| {
                schema
                    .iter()
                    .map(|(_col, path)| {
                        let val = navigate_path(elem, path);
                        match val {
                            Some(Value::Null) | None => "NULL".into(),
                            Some(Value::String(s)) => s,
                            Some(v) => v.to_string(),
                        }
                    })
                    .collect()
            })
            .collect(),
        _ => Vec::new(),
    }
}

/// FOR JSON PATH: build a JSON array from rows.
/// Each row is a Vec of (column_name, value_string) pairs.
pub fn for_json_path(rows: &[Vec<(String, String)>]) -> String {
    let arr: Vec<Value> = rows
        .iter()
        .map(|row| {
            let mut obj = serde_json::Map::new();
            for (col, val) in row {
                // Try to parse the value as JSON; fall back to string.
                let parsed: Value =
                    serde_json::from_str(val).unwrap_or_else(|_| Value::String(val.clone()));
                obj.insert(col.clone(), parsed);
            }
            Value::Object(obj)
        })
        .collect();
    Value::Array(arr).to_string()
}

// -----------------------------------------------------------------------
// Internal helpers
// -----------------------------------------------------------------------

/// Navigate a JSON path like `$.field.subfield` or `$.field[0]`.
fn navigate_path(v: &Value, path: &str) -> Option<Value> {
    let path = path.trim();
    if !path.starts_with('$') {
        return None;
    }
    let path = &path[1..]; // skip $
    if path.is_empty() {
        return Some(v.clone());
    }
    let mut current = v.clone();
    // Parse path segments: .field or [index]
    let mut chars = path.chars().peekable();
    while chars.peek().is_some() {
        match chars.peek() {
            Some('.') => {
                chars.next(); // consume .
                let mut name = String::new();
                while let Some(&c) = chars.peek() {
                    if c == '.' || c == '[' {
                        break;
                    }
                    name.push(c);
                    chars.next();
                }
                current = current.get(&name)?.clone();
            }
            Some('[') => {
                chars.next(); // consume [
                let mut idx_str = String::new();
                while let Some(&c) = chars.peek() {
                    if c == ']' {
                        chars.next();
                        break;
                    }
                    idx_str.push(c);
                    chars.next();
                }
                let idx: usize = idx_str.parse().ok()?;
                current = current.get(idx)?.clone();
            }
            _ => {
                chars.next();
            }
        }
    }
    Some(current)
}

fn set_path(v: &mut Value, path: &str, new_val: Value) {
    // For simplicity, only handle single-level paths like $.field.
    if let Some(dot_pos) = path.rfind('.') {
        let parent_path = &path[..dot_pos];
        let field = &path[dot_pos + 1..];
        let parent = navigate_path_mut(v, parent_path);
        if let Some(Value::Object(map)) = parent {
            map.insert(field.to_string(), new_val);
        }
    }
}

fn delete_path(v: &mut Value, path: &str) {
    if let Some(dot_pos) = path.rfind('.') {
        let parent_path = &path[..dot_pos];
        let field = &path[dot_pos + 1..];
        let parent = navigate_path_mut(v, parent_path);
        if let Some(Value::Object(map)) = parent {
            map.remove(field);
        }
    }
}

fn append_to_path(v: &mut Value, path: &str, new_val: Value) {
    let target = navigate_path_mut(v, path);
    if let Some(Value::Array(arr)) = target {
        arr.push(new_val);
    }
}

fn navigate_path_mut<'a>(v: &'a mut Value, path: &str) -> Option<&'a mut Value> {
    let path = path.trim();
    if !path.starts_with('$') {
        return None;
    }
    let path = &path[1..];
    if path.is_empty() {
        return Some(v);
    }
    // For simplicity, only handle dot-separated fields (no array indexing).
    let mut current = v;
    for seg in path.split('.') {
        if seg.is_empty() {
            continue;
        }
        match current {
            Value::Object(map) => {
                current = map.get_mut(seg)?;
            }
            _ => return None,
        }
    }
    Some(current)
}

// -----------------------------------------------------------------------
// Tests
// -----------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn json_value_string() {
        let j = r#"{"name":"Alice","age":30}"#;
        assert_eq!(json_value(j, "$.name"), Some("Alice".into()));
        assert_eq!(json_value(j, "$.age"), Some("30".into()));
    }

    #[test]
    fn json_value_nested() {
        let j = r#"{"user":{"name":"Bob"}}"#;
        assert_eq!(json_value(j, "$.user.name"), Some("Bob".into()));
    }

    #[test]
    fn json_value_missing_path() {
        let j = r#"{"name":"Alice"}"#;
        assert_eq!(json_value(j, "$.missing"), None);
    }

    #[test]
    fn json_value_array_index() {
        let j = r#"{"items":[10,20,30]}"#;
        assert_eq!(json_value(j, "$.items[1]"), Some("20".into()));
    }

    #[test]
    fn json_query_object() {
        let j = r#"{"user":{"name":"Bob","age":25}}"#;
        let q = json_query(j, "$.user").unwrap();
        assert!(q.contains("Bob"));
        assert!(q.contains("25"));
    }

    #[test]
    fn json_query_array() {
        let j = r#"{"items":[1,2,3]}"#;
        let q = json_query(j, "$.items").unwrap();
        assert_eq!(q, "[1,2,3]");
    }

    #[test]
    fn json_modify_set_string() {
        let j = r#"{"name":"Alice"}"#;
        let modified = json_modify(j, "$.name", "'Bob'");
        assert!(modified.contains("Bob"));
    }

    #[test]
    fn json_modify_set_number() {
        let j = r#"{"age":30}"#;
        let modified = json_modify(j, "$.age", "25");
        assert!(modified.contains("25"));
    }

    #[test]
    fn json_modify_append_array() {
        let j = r#"{"tags":["a"]}"#;
        let modified = json_modify(j, "append $.tags", "'b'");
        assert!(modified.contains("b"));
    }

    #[test]
    fn json_modify_delete_key() {
        let j = r#"{"name":"Alice","age":30}"#;
        let modified = json_modify(j, "$.age", "NULL");
        assert!(!modified.contains("age"));
    }

    #[test]
    fn is_json_valid() {
        assert!(is_json(r#"{"a":1}"#));
        assert!(is_json(r#"[1,2,3]"#));
        assert!(!is_json("not json"));
    }

    #[test]
    fn json_path_exists_true() {
        let j = r#"{"name":"Alice"}"#;
        assert!(json_path_exists(j, "$.name"));
    }

    #[test]
    fn json_path_exists_false() {
        let j = r#"{"name":"Alice"}"#;
        assert!(!json_path_exists(j, "$.missing"));
    }

    #[test]
    fn openjson_array() {
        let j = r#"[1,2,3]"#;
        let rows = openjson_default(j);
        assert_eq!(rows.len(), 3);
        assert_eq!(rows[0].0, "0");
        assert_eq!(rows[0].1, "1");
    }

    #[test]
    fn openjson_object() {
        let j = r#"{"a":1,"b":2}"#;
        let rows = openjson_default(j);
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0].0, "a");
    }

    #[test]
    fn openjson_with_schema_test() {
        let j = r#"[{"name":"Alice","age":30},{"name":"Bob","age":25}]"#;
        let rows = openjson_with_schema(
            j,
            &[("name".into(), "$.name".into()), ("age".into(), "$.age".into())],
        );
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0][0], "Alice");
        assert_eq!(rows[0][1], "30");
    }

    #[test]
    fn for_json_path_basic() {
        let rows = vec![
            vec![("name".into(), "Alice".into()), ("age".into(), "30".into())],
            vec![("name".into(), "Bob".into()), ("age".into(), "25".into())],
        ];
        let json_str = for_json_path(&rows);
        assert!(json_str.starts_with("["));
        assert!(json_str.contains("Alice"));
        assert!(json_str.contains("Bob"));
    }
}
