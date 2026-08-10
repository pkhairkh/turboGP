//! Wave 9 — JSON function integration tests.

use turbogp::exec::json::*;

#[test]
fn json_value_extract_string() {
    let j = r#"{"name":"Alice","age":30,"active":true}"#;
    assert_eq!(json_value(j, "$.name"), Some("Alice".into()));
    assert_eq!(json_value(j, "$.age"), Some("30".into()));
    assert_eq!(json_value(j, "$.active"), Some("true".into()));
}

#[test]
fn json_value_nested_path() {
    let j = r#"{"user":{"profile":{"name":"Bob","email":"bob@test.com"}}}"#;
    assert_eq!(json_value(j, "$.user.profile.name"), Some("Bob".into()));
    assert_eq!(json_value(j, "$.user.profile.email"), Some("bob@test.com".into()));
}

#[test]
fn json_value_array_access() {
    let j = r#"{"items":["apple","banana","cherry"]}"#;
    assert_eq!(json_value(j, "$.items[0]"), Some("apple".into()));
    assert_eq!(json_value(j, "$.items[2]"), Some("cherry".into()));
}

#[test]
fn json_query_returns_object() {
    let j = r#"{"user":{"name":"Bob","age":25},"tags":["a","b"]}"#;
    let q = json_query(j, "$.user").unwrap();
    assert!(q.contains("Bob"));
    assert!(q.contains("25"));
    let q = json_query(j, "$.tags").unwrap();
    assert!(q.starts_with("["));
}

#[test]
fn json_modify_update_field() {
    let j = r#"{"name":"Alice","age":30}"#;
    let modified = json_modify(j, "$.name", "'Bob'");
    assert!(modified.contains("Bob"));
    assert!(!modified.contains("Alice"));
}

#[test]
fn json_modify_append_to_array() {
    let j = r#"{"tags":["work"]}"#;
    let modified = json_modify(j, "append $.tags", "'personal'");
    assert!(modified.contains("work"));
    assert!(modified.contains("personal"));
}

#[test]
fn json_modify_delete_field() {
    let j = r#"{"name":"Alice","age":30}"#;
    let modified = json_modify(j, "$.age", "NULL");
    assert!(!modified.contains("age"));
    assert!(modified.contains("Alice"));
}

#[test]
fn is_json_validates() {
    assert!(is_json(r#"{"a":1}"#));
    assert!(is_json(r#"[1,2,3]"#));
    assert!(is_json(r#""hello""#));
    assert!(is_json("42"));
    assert!(is_json("true"));
    assert!(!is_json("not json"));
    assert!(!is_json("{invalid"));
}

#[test]
fn json_path_exists_check() {
    let j = r#"{"user":{"name":"Bob"}}"#;
    assert!(json_path_exists(j, "$.user"));
    assert!(json_path_exists(j, "$.user.name"));
    assert!(!json_path_exists(j, "$.missing"));
    assert!(!json_path_exists(j, "$.user.missing"));
}

#[test]
fn openjson_array_to_rows() {
    let j = r#"[{"id":1,"name":"Alice"},{"id":2,"name":"Bob"}]"#;
    let rows =
        openjson_with_schema(j, &[("id".into(), "$.id".into()), ("name".into(), "$.name".into())]);
    assert_eq!(rows.len(), 2);
    assert_eq!(rows[0][0], "1");
    assert_eq!(rows[0][1], "Alice");
    assert_eq!(rows[1][0], "2");
    assert_eq!(rows[1][1], "Bob");
}

#[test]
fn for_json_path_builds_array() {
    let rows = vec![
        vec![("id".into(), "1".into()), ("name".into(), "Alice".into())],
        vec![("id".into(), "2".into()), ("name".into(), "Bob".into())],
    ];
    let json_str = for_json_path(&rows);
    assert!(json_str.starts_with("["));
    assert!(json_str.ends_with("]"));
    assert!(json_str.contains("Alice"));
    assert!(json_str.contains("Bob"));
}

#[test]
fn json_value_null_for_object() {
    let j = r#"{"user":{"name":"Bob"}}"#;
    // JSON_VALUE returns NULL for objects (use JSON_QUERY instead).
    assert_eq!(json_value(j, "$.user"), None);
}

#[test]
fn openjson_default_array() {
    let j = r#"[10,20,30]"#;
    let rows = openjson_default(j);
    assert_eq!(rows.len(), 3);
    assert_eq!(rows[0].0, "0");
    assert_eq!(rows[0].1, "10");
    assert_eq!(rows[2].0, "2");
    assert_eq!(rows[2].1, "30");
}
