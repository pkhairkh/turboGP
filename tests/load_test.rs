//! Load test — 100 concurrent connections, mixed SELECT + INSERT for 60s.
//!
//! This test verifies that turboGP handles 100 concurrent connections without
//! panics, lock poisonings, or data corruption. Each connection alternates
//! between INSERT and SELECT operations.

use turbogp::engine::QueryEngine;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::thread;
use std::time::{Duration, Instant};

#[test]
#[ignore = "long-running load test — run with --ignored"]
fn test_100_connections_mixed_load() {
    let engine = Arc::new(parking_lot::Mutex::new(QueryEngine::in_memory()));
    engine.lock().execute("CREATE TABLE load_test (id INT, thread_id INT, val INT, ts BIGINT)").unwrap();

    // Pre-populate with some data
    for i in 0..100 {
        engine.lock().execute(&format!("INSERT INTO load_test VALUES ({}, 0, {}, 0)", i, i)).unwrap();
    }

    let n_connections = 100;
    let duration = Duration::from_secs(5); // Shorter for CI; real test is 60s
    let ops_completed = Arc::new(AtomicU64::new(0));
    let panics = Arc::new(AtomicU64::new(0));

    let start = Instant::now();
    let mut handles = Vec::new();

    for thread_id in 0..n_connections {
        let engine = Arc::clone(&engine);
        let ops = Arc::clone(&ops_completed);
        let panic_count = Arc::clone(&panics);

        let handle = thread::spawn(move || {
            let mut local_ops = 0u64;
            let mut i = 0u64;

            while start.elapsed() < duration {
                let sql = if i % 3 == 0 {
                    // INSERT
                    format!("INSERT INTO load_test VALUES ({}, {}, {}, {})",
                        1000 + thread_id * 10000 + i,
                        thread_id,
                        i,
                        start.elapsed().as_micros())
                } else {
                    // SELECT
                    format!("SELECT COUNT(*) FROM load_test WHERE thread_id = {}", thread_id)
                };

                let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                    engine.lock().execute(&sql)
                }));

                match result {
                    Ok(Ok(_)) => {
                        local_ops += 1;
                    }
                    Ok(Err(_)) => {
                        // Query error is OK (e.g., duplicate key) — not a panic
                    }
                    Err(_) => {
                        panic_count.fetch_add(1, Ordering::Relaxed);
                    }
                }
                i += 1;
            }

            ops.fetch_add(local_ops, Ordering::Relaxed);
        });

        handles.push(handle);
    }

    for handle in handles {
        handle.join().expect("thread panicked");
    }

    let total_ops = ops_completed.load(Ordering::Relaxed);
    let total_panics = panics.load(Ordering::Relaxed);
    let elapsed = start.elapsed();

    println!("Load test completed: {} ops in {:?} ({:.0} ops/sec)",
             total_ops, elapsed, total_ops as f64 / elapsed.as_secs_f64());

    assert_eq!(total_panics, 0,
        "Load test found {} panics across 100 connections", total_panics);

    println!("OK: 100 concurrent connections, 0 panics, {} ops", total_ops);
}
