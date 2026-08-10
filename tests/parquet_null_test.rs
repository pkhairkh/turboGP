//! Wave 58c — Real Parquet NULL test.
//!
//! Writes a small Parquet file with an Int64 column containing NULL values,
//! loads it via `engine.load_parquet()`, runs `SELECT count(col) FROM t`, and
//! verifies that NULLs are excluded from the count.
//!
//! This test does NOT build a synthetic `LoadedColumn` — it uses the actual
//! Parquet loader (`read_parquet`) and the actual engine execution path, so
//! it exercises the full NULL-handling pipeline: arrow → parquet → u64 cells
//! + null_bitmap → executor COUNT(col) → NULL exclusion.

use arrow::array::Int64Array;
use arrow::datatypes::{DataType, Field, Schema};
use arrow::record_batch::RecordBatch;
use parquet::arrow::arrow_writer::ArrowWriter;
use std::fs::File;
use std::sync::Arc;
use tempfile::NamedTempFile;
use turbogp::engine::QueryEngine;

/// Write a RecordBatch to a Parquet file at the given path.
fn write_parquet(path: &str, batch: &RecordBatch) -> Result<(), Box<dyn std::error::Error>> {
    let file = File::create(path)?;
    let mut writer = ArrowWriter::try_new(file, batch.schema(), None)?;
    writer.write(batch)?;
    writer.close()?;
    Ok(())
}

/// Build a RecordBatch with one Int64 column `v` containing:
///   [1, NULL, 3, NULL, 5]
/// The column is nullable (Field::nullable = true).
fn batch_with_nulls() -> RecordBatch {
    // Use Option<i64> to represent nullable Int64 values.
    let values: Vec<Option<i64>> = vec![Some(1), None, Some(3), None, Some(5)];
    let arr = Arc::new(Int64Array::from(values));
    let schema = Arc::new(Schema::new(vec![Field::new("v", DataType::Int64, true)]));
    RecordBatch::try_new(schema, vec![arr]).expect("build batch")
}

#[test]
fn count_excludes_nulls() {
    let tmp = NamedTempFile::new().expect("temp file");
    let path = tmp.path().to_str().expect("path str");
    let batch = batch_with_nulls();
    write_parquet(path, &batch).expect("write parquet");

    let mut e = QueryEngine::in_memory();
    let n = e.load_parquet(path, "t").expect("load parquet");
    assert_eq!(n, 5, "parquet file must have 5 rows");

    // count(v) must exclude NULLs: 3 non-NULL values (1, 3, 5).
    let r = e.execute("SELECT count(v) FROM t").expect("count query");
    assert_eq!(
        r.scalar_u64(),
        Some(3),
        "count(v) must exclude NULLs — expected 3, got: {:?}",
        r.scalar_u64()
    );

    // count(*) must include NULLs: 5 rows total.
    let r = e.execute("SELECT count(*) FROM t").expect("count(*) query");
    assert_eq!(
        r.scalar_u64(),
        Some(5),
        "count(*) must include NULLs — expected 5, got: {:?}",
        r.scalar_u64()
    );
}

/// Verify that SUM and AVG also exclude NULLs.
#[test]
fn sum_and_avg_exclude_nulls() {
    let tmp = NamedTempFile::new().expect("temp file");
    let path = tmp.path().to_str().expect("path str");
    let batch = batch_with_nulls();
    write_parquet(path, &batch).expect("write parquet");

    let mut e = QueryEngine::in_memory();
    e.load_parquet(path, "t").expect("load parquet");

    // SUM(v) = 1 + 3 + 5 = 9 (NULLs excluded).
    let r = e.execute("SELECT sum(v) FROM t").expect("sum query");
    let sum_val = r.scalar_f64().expect("expected f64 result");
    assert!(
        (sum_val - 9.0).abs() < 0.01,
        "sum(v) must exclude NULLs — expected 9.0, got: {}",
        sum_val
    );

    // AVG(v) = 9 / 3 = 3.0 (NULLs excluded from both numerator and denominator).
    let r = e.execute("SELECT avg(v) FROM t").expect("avg query");
    let avg_val = r.scalar_f64().expect("expected f64 result");
    assert!(
        (avg_val - 3.0).abs() < 0.01,
        "avg(v) must exclude NULLs — expected 3.0, got: {}",
        avg_val
    );
}
