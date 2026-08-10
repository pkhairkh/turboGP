//! SQL injection negative tests for the pgwire Bind/Execute parameter path.
//!
//! These tests verify that bound parameters ($1, $2, ...) cannot break out
//! of the SQL statement to inject arbitrary code. The fix is in
//! `src/server/pgwire.rs::escape_param_value` which doubles single quotes
//! in non-numeric parameter values.

use turbogp::server::pgwire;

#[test]
fn test_single_quote_breakout_is_escaped() {
    // Classic: '); DROP TABLE x; --
    let sql = "INSERT INTO t VALUES ($1)";
    let params = vec!["'); DROP TABLE t; --".to_string()];
    let result = pgwire::substitute_params(sql, &params);
    // The param should be wrapped in quotes with internal quotes doubled.
    // The result should NOT contain an unescaped DROP TABLE outside a string.
    assert!(
        result.contains("'''); DROP TABLE t; --'"),
        "expected escaped single-quote, got: {result}"
    );
    // Verify the DROP is inside a string literal (between quotes), not executable.
    // The full value should be: INSERT INTO t VALUES (''); DROP TABLE t; --')
    assert!(!result.ends_with("; --"), "injection appears to have broken out: {result}");
}

#[test]
fn test_semicolon_injection_is_escaped() {
    let sql = "SELECT * FROM t WHERE name = $1";
    let params = vec!["a; DROP TABLE t".to_string()];
    let result = pgwire::substitute_params(sql, &params);
    // The semicolon should be inside a quoted string, not a statement separator.
    assert!(result.contains("'a; DROP TABLE t'"), "expected quoted value, got: {result}");
}

#[test]
fn test_comment_injection_is_escaped() {
    // -- comment injection
    let sql = "SELECT * FROM t WHERE name = $1";
    let params = vec!["admin' --".to_string()];
    let result = pgwire::substitute_params(sql, &params);
    assert!(result.contains("'admin'' --'"), "expected doubled quote, got: {result}");
}

#[test]
fn test_unicode_evasion_is_escaped() {
    // Unicode right single quote (U+2019) should not be treated as a terminator.
    // It's not a ASCII single quote so it passes through, but the value is still
    // wrapped in quotes.
    let sql = "SELECT * FROM t WHERE name = $1";
    let params = vec!["O\u{2019}Brien".to_string()];
    let result = pgwire::substitute_params(sql, &params);
    assert!(result.contains("'O\u{2019}Brien'"), "expected quoted unicode value, got: {result}");
}

#[test]
fn test_nested_quote_attack_is_escaped() {
    // Attempt to close the string with '' and inject.
    let sql = "SELECT * FROM t WHERE name = $1";
    let params = vec!["''.toString()//".to_string()];
    let result = pgwire::substitute_params(sql, &params);
    // Both single quotes should be doubled.
    assert!(result.contains("''''.toString()//'"), "expected all quotes doubled, got: {result}");
}

#[test]
fn test_numeric_param_passes_through() {
    let sql = "SELECT * FROM t WHERE id = $1";
    let params = vec!["42".to_string()];
    let result = pgwire::substitute_params(sql, &params);
    assert_eq!(result, "SELECT * FROM t WHERE id = 42");
}

#[test]
fn test_float_param_passes_through() {
    let sql = "SELECT * FROM t WHERE price = $1";
    let params = vec!["3.14".to_string()];
    let result = pgwire::substitute_params(sql, &params);
    assert_eq!(result, "SELECT * FROM t WHERE price = 3.14");
}

#[test]
fn test_multiple_params() {
    let sql = "SELECT * FROM t WHERE a = $1 AND b = $2 AND c = $3";
    let params = vec![
        "42".to_string(),          // numeric
        "hello".to_string(),       // string
        "it's a test".to_string(), // string with quote
    ];
    let result = pgwire::substitute_params(sql, &params);
    assert!(result.contains("a = 42"));
    assert!(result.contains("b = 'hello'"));
    assert!(result.contains("c = 'it''s a test'"));
}
