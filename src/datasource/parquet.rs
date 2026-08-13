//! # Parquet reader — load `.parquet` files into u64 columns.
//!
//! Uses the `parquet` and `arrow` crates (both pinned at v55 in
//! `Cargo.toml`) to read RecordBatches, then converts each Arrow
//! column into turboGP's universal `Vec<u64>` cell format.
//!
//! ## Why Parquet
//!
//! ClickBench ships its data as Parquet. DuckDB's TPC-H generator
//! emits Parquet. Without a Parquet reader turboGP can run neither
//! benchmark suite, which would leave every performance claim in
//! `docs/3x-proof.md` unverifiable on external data.
//!
//! ## Why `Vec<u64>`
//!
//! The kernel table (see [`crate::kernel`]) operates on 64-bit cells.
//! Every operator — `scan_eq`, `hash_probe`, `aggregate_sum` —
//! consumes and produces `u64` words. Loading Parquet data into the
//! same shape means no per-operator conversion layer is needed: a
//! column read from Parquet can be fed directly to a kernel.
//!
//! ## Type conversion table
//!
//! See the table in [`crate::datasource`] for the full mapping. Null
//! values are tracked via the `null_bitmap` field on `LoadedColumn`
//! (Wave 46) — the bitmap marks which cells are NULL, and the dispatch
//! path consults it so `COUNT(col)` excludes NULLs and pgwire sends
//! NULL as a `-1` length indicator (Wave 52).

use arrow::array::{
    Array, ArrayRef, BooleanArray, Date32Array, Float64Array, Int16Array, Int32Array, Int64Array,
    Int8Array, StringArray, UInt16Array, UInt32Array, UInt64Array, UInt8Array,
};
use arrow::datatypes::DataType;
use arrow::record_batch::RecordBatch;
use parquet::arrow::arrow_reader::ParquetRecordBatchReaderBuilder;
use parquet::arrow::arrow_writer::ArrowWriter;
use std::error::Error;
use std::fs::File;
use xxhash_rust::xxh3;

/// A column loaded from Parquet, already converted to turboGP's u64
/// cell format.
///
/// `cells.len() == row_count`. The column name is preserved so the
/// loader can hand the column to a [`crate::catalog::Catalog`] under
/// the right field.
#[derive(Debug, Clone)]
pub struct LoadedColumn {
    /// Column name (taken from the Parquet schema).
    pub name: String,
    /// Data as u64 cells (the engine's universal format).
    pub cells: Vec<u64>,
    /// Number of rows. Always equal to `cells.len()`.
    pub row_count: usize,
    /// For string columns: actual string data for LIKE queries.
    pub string_search: Option<crate::exec::fm_index::StringSearchColumn>,
    /// NULL bitmap: true = cell is NULL. None if no NULLs in this column (Wave 46).
    pub null_bitmap: Option<Vec<bool>>,
}

/// A table loaded from Parquet — a name plus a `Vec<LoadedColumn>`.
///
/// Every column has the same `row_count`; the loader verifies this
/// invariant and returns an error otherwise.
#[derive(Debug, Clone)]
pub struct LoadedTable {
    /// Table name (caller-supplied — Parquet has no table name).
    pub name: String,
    /// Columns in schema order.
    pub columns: Vec<LoadedColumn>,
    /// Number of rows. Equal to every column's `row_count`.
    pub row_count: usize,
    /// Optional i32 sidecar for narrow integer columns
    /// (Int/SmallInt/TinyInt). Populated by the CSV loader when all
    /// values fit in i32 range. None for columns that are u64/f64/string
    /// or have values outside i32 range. When present, the filter path
    /// uses `filter_eq_i32` etc. (4 bytes/element vs 8 for u64),
    /// halving memory bandwidth. Parallel to `columns`; entries past
    /// `columns.len()` (or when None) mean no sidecar for that column.
    /// Wave 5C.
    pub i32_columns: Vec<Option<Vec<i32>>>,
}

impl LoadedTable {
    /// Derive the table name from the file path's stem
    /// (`hits.parquet` → `hits`). Used when callers don't supply a
    /// table name explicitly.
    pub fn name_from_path(path: &str) -> String {
        let stem = path.rsplit('/').next().unwrap_or(path);
        stem.rsplit_once('.').map(|(s, _)| s).unwrap_or(stem).to_string()
    }
}

/// Read a Parquet file and return all columns as u64 cells.
///
/// Iterates every row group, concatenates batches into a single
/// `Vec<u64>` per column, and verifies every column has the same
/// length.
///
/// # Errors
///
/// Returns an error if:
/// - the file cannot be opened,
/// - the Parquet metadata cannot be parsed,
/// - a batch read fails,
/// - the schema contains an unsupported Arrow type, or
/// - the resulting columns have mismatched lengths.
pub fn read_parquet(path: &str) -> Result<LoadedTable, Box<dyn Error>> {
    let file = File::open(path)?;
    let builder = ParquetRecordBatchReaderBuilder::try_new(file)?;
    let reader = builder.with_batch_size(8192).build()?;

    let name = LoadedTable::name_from_path(path);

    // Accumulators: one Vec<u64> per column, plus the column names
    // captured from the first batch's schema.
    let mut col_cells: Vec<Vec<u64>> = Vec::new();
    let mut col_names: Vec<String> = Vec::new();
    let mut col_strings: Vec<Vec<String>> = Vec::new();
    // Per-column NULL bitmap accumulator (Wave 46).
    let mut col_nulls: Vec<Option<Vec<bool>>> = Vec::new();
    let mut total_rows: usize = 0;

    for batch in reader {
        let batch: RecordBatch = batch?;
        let schema = batch.schema();

        // First batch: size the accumulators from the schema.
        if col_cells.is_empty() {
            for field in schema.fields() {
                col_names.push(field.name().to_string());
                col_cells.push(Vec::new());
                col_strings.push(Vec::new());
                col_nulls.push(None);
            }
        }

        for (i, col) in batch.columns().iter().enumerate() {
            // Each batch must have one value per column for every row.
            let (cells, string_search, null_bitmap) = convert_array_to_u64(col);
            col_cells[i].extend(cells);
            if let Some(ss) = string_search {
                col_strings[i].extend(ss.strings);
            }
            // Propagate NULL bitmap (Wave 46).
            if let Some(nb) = null_bitmap {
                if col_nulls[i].is_none() {
                    col_nulls[i] = Some(Vec::new());
                }
                col_nulls[i].as_mut().unwrap().extend(nb);
            }
        }
        total_rows += batch.num_rows();
    }

    if col_cells.is_empty() {
        // Empty file — return an empty table with the inferred name.
        return Ok(LoadedTable { name, columns: Vec::new(), row_count: 0, i32_columns: Vec::new() });
    }

    // Verify every column has the same length.
    for (i, cells) in col_cells.iter().enumerate() {
        if cells.len() != total_rows {
            return Err(format!(
                "parquet column '{}' has {} cells but row_count is {}",
                col_names[i],
                cells.len(),
                total_rows
            )
            .into());
        }
    }

    let mut columns: Vec<LoadedColumn> = Vec::with_capacity(col_cells.len());
    for i in 0..col_cells.len() {
        // Take ownership of the accumulated strings without cloning. For
        // non-string columns `col_strings[i]` is empty → `string_search`
        // stays `None`.
        let string_search = if !col_strings[i].is_empty() {
            Some(crate::exec::fm_index::StringSearchColumn::new(std::mem::take(
                &mut col_strings[i],
            )))
        } else {
            None
        };
        columns.push(LoadedColumn {
            name: col_names[i].clone(),
            row_count: total_rows,
            cells: std::mem::take(&mut col_cells[i]),
            string_search,
            null_bitmap: col_nulls[i].take(),
        });
    }

    Ok(LoadedTable { name, columns, row_count: total_rows, i32_columns: Vec::new() })
}

/// Read a single column from a Parquet file.
///
/// Cheaper than [`read_parquet`] when only one column is needed: the
/// `parquet` crate still materialises every column of every row group
/// (column pruning is a row-group-level option, not a ParquetReader
/// option), but we avoid the per-column allocation and length check
/// for the columns the caller does not want.
///
/// # Errors
///
/// Same as [`read_parquet`], plus `column_name` not found in the
/// schema.
/// Read only the column names from a Parquet file (metadata-only, no data read).
/// Used by column pruning (Wave 30) to determine which columns to load.
pub fn read_parquet_column_names(path: &str) -> Result<Vec<String>, Box<dyn Error>> {
    let file = File::open(path)?;
    let builder = ParquetRecordBatchReaderBuilder::try_new(file)?;
    let schema = builder.schema();
    Ok(schema.fields().iter().map(|f| f.name().to_string()).collect())
}

pub fn read_parquet_column(path: &str, column_name: &str) -> Result<LoadedColumn, Box<dyn Error>> {
    let file = File::open(path)?;
    let builder = ParquetRecordBatchReaderBuilder::try_new(file)?;

    // Resolve the column index up front so we can fail fast.
    let schema = builder.schema();
    let col_idx = schema
        .column_with_name(column_name)
        .ok_or_else(|| format!("column '{column_name}' not found in parquet schema"))?
        .0;

    let reader = builder.with_batch_size(8192).build()?;

    let mut cells: Vec<u64> = Vec::new();
    let mut strings: Vec<String> = Vec::new();
    let mut row_count: usize = 0;
    for batch in reader {
        let batch: RecordBatch = batch?;
        let arr: &ArrayRef = batch.column(col_idx);
        let (new_cells, string_search, _null_bitmap) = convert_array_to_u64(arr);
        cells.extend(new_cells);
        if let Some(ss) = string_search {
            strings.extend(ss.strings);
        }
        row_count += batch.num_rows();
    }

    let string_search = if !strings.is_empty() {
        Some(crate::exec::fm_index::StringSearchColumn::new(strings))
    } else {
        None
    };
    Ok(LoadedColumn {
        name: column_name.to_string(),
        cells,
        row_count,
        string_search,
        null_bitmap: None,
    })
}

/// Convert an Arrow [`ArrayRef`] into turboGP's `Vec<u64>` cell format.
///
/// The conversion is total — every Arrow type that appears in the
/// ClickBench / TPC-H datasets is handled. Unknown types fall back to
/// `0u64` for every row (with a comment) so the engine never panics
/// on an exotic type; the caller can detect this by inspecting the
/// schema separately if needed.
///
/// Null values are encoded as `0u64` (the sentinel — see the module
/// docs).
fn convert_array_to_u64(
    array: &ArrayRef,
) -> (Vec<u64>, Option<crate::exec::fm_index::StringSearchColumn>, Option<Vec<bool>>) {
    let len = array.len();
    let mut out = Vec::with_capacity(len);
    let mut string_search: Option<crate::exec::fm_index::StringSearchColumn> = None;

    match array.data_type() {
        // Int8/Int16/Int32 → `value as u64` (sign-extends negative
        // values to large u64 — same bit pattern the kernel compares).
        // Int8 and Int16 are common in ClickBench (TraficSourceID,
        // SearchEngineID, etc.) and were previously dropped to 0 by
        // the unsupported-type fallback, which silently broke GROUP BY
        // and equality filters on those columns.
        DataType::Int8 => {
            if let Some(a) = array.as_any().downcast_ref::<Int8Array>() {
                for i in 0..len {
                    if a.is_null(i) {
                        out.push(0);
                    } else {
                        out.push(a.value(i) as u64);
                    }
                }
            }
        }
        DataType::Int16 => {
            if let Some(a) = array.as_any().downcast_ref::<Int16Array>() {
                for i in 0..len {
                    if a.is_null(i) {
                        out.push(0);
                    } else {
                        out.push(a.value(i) as u64);
                    }
                }
            }
        }
        // Int32 → `value as u64` (zero-extends; negative values
        // become large u64 — same bit pattern the kernel compares).
        DataType::Int32 => {
            if let Some(a) = array.as_any().downcast_ref::<Int32Array>() {
                for i in 0..len {
                    if a.is_null(i) {
                        out.push(0);
                    } else {
                        out.push(a.value(i) as u64);
                    }
                }
            }
        }
        // Int64 → `value as u64` (bit-reinterpret).
        DataType::Int64 => {
            if let Some(a) = array.as_any().downcast_ref::<Int64Array>() {
                for i in 0..len {
                    if a.is_null(i) {
                        out.push(0);
                    } else {
                        out.push(a.value(i) as u64);
                    }
                }
            }
        }
        // UInt8/UInt16/UInt32/UInt64 → `value as u64` (zero-extends).
        DataType::UInt8 => {
            if let Some(a) = array.as_any().downcast_ref::<UInt8Array>() {
                for i in 0..len {
                    if a.is_null(i) {
                        out.push(0);
                    } else {
                        out.push(a.value(i) as u64);
                    }
                }
            }
        }
        DataType::UInt16 => {
            if let Some(a) = array.as_any().downcast_ref::<UInt16Array>() {
                for i in 0..len {
                    if a.is_null(i) {
                        out.push(0);
                    } else {
                        out.push(a.value(i) as u64);
                    }
                }
            }
        }
        DataType::UInt32 => {
            if let Some(a) = array.as_any().downcast_ref::<UInt32Array>() {
                for i in 0..len {
                    if a.is_null(i) {
                        out.push(0);
                    } else {
                        out.push(a.value(i) as u64);
                    }
                }
            }
        }
        DataType::UInt64 => {
            if let Some(a) = array.as_any().downcast_ref::<UInt64Array>() {
                for i in 0..len {
                    if a.is_null(i) {
                        out.push(0);
                    } else {
                        out.push(a.value(i));
                    }
                }
            }
        }
        // Float64 → `f64::to_bits(value)` (preserves the bit pattern).
        DataType::Float64 => {
            if let Some(a) = array.as_any().downcast_ref::<Float64Array>() {
                for i in 0..len {
                    if a.is_null(i) {
                        out.push(0);
                    } else {
                        out.push(a.value(i).to_bits());
                    }
                }
            }
        }
        // Utf8 → xxh3_64(bytes) hash. Lossy: the engine can filter on
        // equality of the hash but cannot recover the original
        // string. Full string support deferred to a future wave.
        DataType::Utf8 => {
            if let Some(a) = array.as_any().downcast_ref::<StringArray>() {
                let mut strings: Vec<String> = Vec::with_capacity(len);
                for i in 0..len {
                    if a.is_null(i) {
                        out.push(0);
                        strings.push(String::new());
                    } else {
                        let val = a.value(i);
                        out.push(xxh3::xxh3_64(val.as_bytes()));
                        strings.push(val.to_string());
                    }
                }
                string_search = Some(crate::exec::fm_index::StringSearchColumn::new(strings));
            }
        }
        // LargeUtf8 — same as Utf8; the only difference is the offset
        // width, which StringArray hides behind `value(i)`.
        DataType::LargeUtf8 => {
            if let Some(a) = array.as_any().downcast_ref::<StringArray>() {
                let mut strings: Vec<String> = Vec::with_capacity(len);
                for i in 0..len {
                    if a.is_null(i) {
                        out.push(0);
                        strings.push(String::new());
                    } else {
                        let val = a.value(i);
                        out.push(xxh3::xxh3_64(val.as_bytes()));
                        strings.push(val.to_string());
                    }
                }
                string_search = Some(crate::exec::fm_index::StringSearchColumn::new(strings));
            }
        }
        // Boolean → 0u64 / 1u64.
        DataType::Boolean => {
            if let Some(a) = array.as_any().downcast_ref::<BooleanArray>() {
                for i in 0..len {
                    if a.is_null(i) {
                        out.push(0);
                    } else {
                        out.push(if a.value(i) { 1 } else { 0 });
                    }
                }
            }
        }
        // Date32 → days since the Unix epoch as u64.
        DataType::Date32 => {
            if let Some(a) = array.as_any().downcast_ref::<Date32Array>() {
                for i in 0..len {
                    if a.is_null(i) {
                        out.push(0);
                    } else {
                        out.push(a.value(i) as u64);
                    }
                }
            }
        }
        // Unsupported type — fill with zeros. The caller can detect
        // this by inspecting the schema before calling the reader.
        _ => {
            out.resize(len, 0);
        }
    }

    // If a `downcast_ref` returned None for a matched DataType (which
    // shouldn't happen but is defensive), ensure the output is the
    // right length.
    if out.len() != len {
        out.resize(len, 0);
    }

    // Build NULL bitmap: check if any nulls exist in the array.
    let null_bitmap = if array.null_count() > 0 {
        let mut bits = Vec::with_capacity(len);
        for i in 0..len {
            bits.push(array.is_null(i));
        }
        Some(bits)
    } else {
        None
    };

    (out, string_search, null_bitmap)
}

/// Write a `RecordBatch` to a Parquet file at `path`. Used by tests
/// to manufacture small fixtures without checking in binary assets.
///
/// Exported as `pub` so integration tests in `tests/` can use it.
pub fn write_parquet_for_test(path: &str, batch: &RecordBatch) -> Result<(), Box<dyn Error>> {
    let file = File::create(path)?;
    let mut writer = ArrowWriter::try_new(file, batch.schema(), None)?;
    writer.write(batch)?;
    writer.close()?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use arrow::array::{Float64Array, Int32Array, Int64Array, StringArray};
    use arrow::datatypes::{DataType, Field, Schema};
    use std::sync::Arc;
    use tempfile::NamedTempFile;

    /// Build a `RecordBatch` with three columns: Int64 `id`, Float64
    /// `score`, Utf8 `label`.
    fn sample_batch(n: i64) -> RecordBatch {
        let ids: Vec<i64> = (0..n).collect();
        let scores: Vec<f64> = (0..n).map(|i| i as f64 * 0.5).collect();
        let labels: Vec<&str> = (0..n).map(|_| "row").collect();

        let id_arr = Arc::new(Int64Array::from(ids));
        let score_arr = Arc::new(Float64Array::from(scores));
        let label_arr = Arc::new(StringArray::from(labels));

        let schema = Arc::new(Schema::new(vec![
            Field::new("id", DataType::Int64, false),
            Field::new("score", DataType::Float64, false),
            Field::new("label", DataType::Utf8, false),
        ]));

        RecordBatch::try_new(schema, vec![id_arr, score_arr, label_arr]).expect("build batch")
    }

    /// Round-trip a small Parquet file: write Int64/Float64/Utf8,
    /// read it back, verify the cells match.
    #[test]
    fn read_parquet_round_trip() {
        let tmp = NamedTempFile::new().expect("temp file");
        let path = tmp.path().to_str().expect("path str");
        let batch = sample_batch(100);
        write_parquet_for_test(path, &batch).expect("write");

        let table = read_parquet(path).expect("read");

        assert_eq!(table.row_count, 100);
        assert_eq!(table.columns.len(), 3);

        // id column: Int64 → `value as u64`.
        let id_col = &table.columns[0];
        assert_eq!(id_col.name, "id");
        assert_eq!(id_col.cells.len(), 100);
        for i in 0..100i64 {
            assert_eq!(id_col.cells[i as usize], i as u64);
        }

        // score column: Float64 → `to_bits`.
        let score_col = &table.columns[1];
        for i in 0..100i64 {
            let expected = (i as f64 * 0.5).to_bits();
            assert_eq!(score_col.cells[i as usize], expected);
        }

        // label column: Utf8 → xxh3_64("row"). All 100 cells equal.
        let label_col = &table.columns[2];
        let expected_hash = xxh3::xxh3_64(b"row");
        for i in 0..100 {
            assert_eq!(label_col.cells[i], expected_hash);
        }
    }

    /// `read_parquet_column` returns only the requested column.
    #[test]
    fn read_parquet_single_column() {
        let tmp = NamedTempFile::new().expect("temp file");
        let path = tmp.path().to_str().expect("path str");
        let batch = sample_batch(50);
        write_parquet_for_test(path, &batch).expect("write");

        let col = read_parquet_column(path, "score").expect("read column");
        assert_eq!(col.name, "score");
        assert_eq!(col.row_count, 50);
        assert_eq!(col.cells.len(), 50);
        for i in 0..50i64 {
            assert_eq!(col.cells[i as usize], (i as f64 * 0.5).to_bits());
        }
    }

    /// `read_parquet_column` errors on an unknown column name.
    #[test]
    fn read_parquet_column_unknown_errors() {
        let tmp = NamedTempFile::new().expect("temp file");
        let path = tmp.path().to_str().expect("path str");
        let batch = sample_batch(10);
        write_parquet_for_test(path, &batch).expect("write");

        let err = read_parquet_column(path, "nonexistent").unwrap_err();
        assert!(err.to_string().contains("not found"), "expected 'not found' error, got: {}", err);
    }

    /// A Parquet file with a single Int32 column converts correctly
    /// (verifies the Int32 branch).
    #[test]
    fn read_parquet_int32_column() {
        let tmp = NamedTempFile::new().expect("temp file");
        let path = tmp.path().to_str().expect("path str");

        let arr = Arc::new(Int32Array::from(vec![1i32, 2, 3, 4, 5]));
        let schema = Arc::new(Schema::new(vec![Field::new("x", DataType::Int32, false)]));
        let batch = RecordBatch::try_new(schema, vec![arr]).expect("build batch");
        write_parquet_for_test(path, &batch).expect("write");

        let table = read_parquet(path).expect("read");
        assert_eq!(table.row_count, 5);
        assert_eq!(table.columns[0].cells, vec![1u64, 2, 3, 4, 5]);
    }

    /// `name_from_path` strips the directory and extension.
    #[test]
    fn name_from_path_strips_extension() {
        assert_eq!(LoadedTable::name_from_path("/tmp/hits.parquet"), "hits");
        assert_eq!(LoadedTable::name_from_path("hits.parquet"), "hits");
        assert_eq!(LoadedTable::name_from_path("no_extension"), "no_extension");
    }

    /// `LoadedColumn` and `LoadedTable` derive `Clone` so callers can
    /// cheaply snapshot a loaded table into a `Catalog` while keeping
    /// the original for re-loads.
    #[test]
    fn loaded_types_are_clone() {
        let col = LoadedColumn {
            name: "x".into(),
            cells: vec![1, 2, 3],
            row_count: 3,
            string_search: None,
            null_bitmap: None,
        };
        let col2 = col.clone();
        assert_eq!(col.cells, col2.cells);

        let table = LoadedTable { name: "t".into(), columns: vec![col], row_count: 3, i32_columns: Vec::new() };
        let _table2 = table.clone();
    }
}
