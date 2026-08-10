//! Fuzz test — generates 10,000 random SQL strings and verifies no panics.
//!
//! This test generates pseudo-random SQL by combining:
//! - Random SQL keywords (SELECT, INSERT, CREATE, etc.)
//! - Random identifiers (table names, column names)
//! - Random literals (integers, floats, strings)
//! - Random operators (=, <>, <, >, AND, OR, etc.)
//!
//! The test passes if no input causes a panic (unwrap/expect/panic/unreachable).
//! Parser errors are expected and OK — only panics are failures.

use turbogp::engine::QueryEngine;

#[test]
#[ignore = "long-running fuzz test — run with --ignored"]
fn fuzz_random_sql_no_panics() {
    let mut engine = QueryEngine::in_memory();

    // Simple LCG random number generator (deterministic for reproducibility)
    let mut state: u64 = 0x1234567890ABCDEF;
    let mut next_rand = || {
        state = state.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
        state >> 33
    };

    let keywords = [
        "SELECT", "FROM", "WHERE", "INSERT", "INTO", "VALUES", "UPDATE",
        "SET", "DELETE", "CREATE", "TABLE", "INDEX", "DROP", "AND", "OR",
        "NOT", "NULL", "COUNT", "SUM", "AVG", "MIN", "MAX", "GROUP", "BY",
        "ORDER", "LIMIT", "JOIN", "ON", "AS", "DISTINCT", "HAVING",
    ];
    let identifiers = [
        "users", "orders", "items", "t", "a", "b", "c", "id", "name",
        "price", "qty", "date", "status", "x", "y", "z", "col1", "col2",
    ];
    let literals = ["1", "42", "100", "3.14", "0.5", "'hello'", "'world'", "NULL", "0", "-1"];
    let operators = ["=", "<>", "<", ">", "<=", ">=", "+", "-", "*", "/"];

    let mut panics = 0;
    let n = 10_000;

    for i in 0..n {
        // Build a random SQL string from 3-10 tokens
        let n_tokens = 3 + (next_rand() % 8) as usize;
        let mut tokens: Vec<&str> = Vec::with_capacity(n_tokens);
        for _ in 0..n_tokens {
            let cat = next_rand() % 4;
            let token = match cat {
                0 => keywords[(next_rand() as usize) % keywords.len()],
                1 => identifiers[(next_rand() as usize) % identifiers.len()],
                2 => literals[(next_rand() as usize) % literals.len()],
                _ => operators[(next_rand() as usize) % operators.len()],
            };
            tokens.push(token);
        }
        let sql = tokens.join(" ");

        // Execute — catch panics with catch_unwind
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let _ = engine.execute(&sql);
        }));

        if result.is_err() {
            panics += 1;
            eprintln!("PANIC on input #{}: {}", i, sql);
        }
    }

    assert_eq!(panics, 0, "Fuzz test found {} panics out of {} inputs", panics, n);
    println!("Fuzz test passed: {} inputs, 0 panics", n);
}
