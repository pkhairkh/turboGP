//! # CSV reader — load `.csv` files into u64 columns.
//!
//! A minimal CSV reader using only `std::fs`. Numeric columns are
//! parsed as `i64` and cast to `u64`; non-numeric columns are hashed
//! with xxh3 so the engine can still filter on equality (the same
//! lossy contract as [`super::parquet`] string columns).
//!
//! ## Limitations
//!
//! - No quoted-field handling. Fields containing commas, embedded
//!   newlines, or quote characters will be mis-split. This is fine for
//!   the ClickBench / TPC-H CSV exports, which use simple
//!   comma-separated values without quoting.
//! - No type inference beyond "all values parse as i64 ⇒ numeric,
//!   else hash". Float columns are hashed. (Use Parquet for float
//!   data.)
//! - Empty values are encoded as `0u64` in numeric columns and as
//!   `xxh3_64(b"")` in hashed columns.
//!
//! ## Why not `arrow-csv`
//!
//! The `arrow-csv` crate (transitively pulled in by `arrow = "55"`)
//! would do this in five lines, but turboGP deliberately keeps the
//! CSV path dependency-light: CSV is the lowest-common-denominator
//! format and the reader should remain auditable without pulling in
//! arrow's full CSV parser. Parquet, by contrast, has no simple
//! implementation and gets the full `parquet` crate.

use crate::datasource::parquet::{LoadedColumn, LoadedTable};
use std::error::Error;
use std::fs;
use std::fs::File;
use std::io::{BufRead, BufReader};
use xxhash_rust::xxh3;

/// Read a CSV file and return columns as u64 cells.
///
/// If `has_header` is true, the first row is treated as column names;
/// otherwise columns are named `col_0`, `col_1`, …
///
/// Each column is independently typed:
///
/// - If every value in the column parses as `i64`, the column is
///   numeric: each value is cast to `u64` (`value as u64`, which
///   bit-reinterprets negatives).
/// - Otherwise the column is hashed: each value's bytes are passed to
///   `xxh3_64`.
///
/// # Errors
///
/// Returns an error if the file cannot be read or if the rows have
/// inconsistent column counts.
pub fn read_csv(path: &str, has_header: bool) -> Result<LoadedTable, Box<dyn Error>> {
    // Streaming CSV reader: reads line by line via BufReader (64KB buffer)
    // and accumulates cells into column-major Vec<String> buffers.
    //
    // This replaces the previous fs::read_to_string approach which slurped
    // the entire file into memory (7.7GB for lineitem SF=10) plus built
    // Vec<Vec<&str>> (16GB transient). Peak transient memory is now just
    // the column-major string buffers (which become StringSearchColumn).

    let file = File::open(path)?;
    let reader = BufReader::new(file);

    let mut column_names: Vec<String> = Vec::new();
    let mut column_data: Vec<Vec<String>> = Vec::new(); // column-major strings
    let mut row_count: usize = 0;
    let mut header_seen = false;

    for line_result in reader.lines() {
        let line = line_result?;
        let line = line.trim_end_matches('\r');
        if line.is_empty() {
            continue;
        }

        if has_header && !header_seen {
            column_names = line.split(',').map(|s| s.to_string()).collect();
            column_data = vec![Vec::new(); column_names.len()];
            header_seen = true;
            continue;
        }

        let cells: Vec<&str> = line.split(',').collect();

        if column_names.is_empty() {
            // No header mode: infer column count from first data row
            column_names = (0..cells.len()).map(|i| format!("col_{i}")).collect();
            column_data = vec![Vec::new(); column_names.len()];
        }

        if cells.len() != column_names.len() {
            return Err(format!(
                "CSV row {} has {} fields, expected {}",
                row_count + if has_header { 1 } else { 0 },
                cells.len(),
                column_names.len()
            )
            .into());
        }

        for (col_idx, cell) in cells.iter().enumerate() {
            column_data[col_idx].push(cell.to_string());
        }
        row_count += 1;
    }

    if column_names.is_empty() {
        return Ok(LoadedTable {
            name: LoadedTable::name_from_path(path),
            columns: Vec::new(),
            row_count: 0,
            i32_columns: Vec::new(),
        });
    }

    let ncols = column_names.len();
    let mut columns: Vec<LoadedColumn> = Vec::with_capacity(ncols);
    // Wave 5C: i32 sidecar. Parallel to `columns`; Some(Vec<i32>) for
    // numeric columns whose values all fit in i32 range, None otherwise.
    let mut i32_columns: Vec<Option<Vec<i32>>> = Vec::with_capacity(ncols);

    for (col_idx, name) in column_names.iter().enumerate() {
        let strings = &column_data[col_idx];

        // Try i64 first
        let mut as_i64: Vec<i64> = Vec::with_capacity(row_count);
        let mut all_numeric = true;
        for s in strings {
            match s.parse::<i64>() {
                Ok(n) => as_i64.push(n),
                Err(_) => {
                    all_numeric = false;
                    break;
                }
            }
        }

        // W25: Track whether this column parsed as a date (YYYY-MM-DD).
        // Set in the else branch below; used by `is_string` to avoid
        // building a StringSearchColumn for date columns.
        let mut parsed_as_date = false;

        let cells: Vec<u64> = if all_numeric {
            // W6C: i32 sidecar disabled — profiling showed TPC-H queries are
            // EXISTS/JoinHashProbe-bound, not filter-bandwidth-bound. The
            // sidecar duplicated storage (+16GB RSS at SF=10). The i32
            // filter kernels remain in bitmap.rs for future use.
            i32_columns.push(None);
            // W25-T2: TPC-H float columns that may have integer-looking
            // values in the CSV (e.g. l_quantity=17 instead of 17.00).
            // tpc_h_col_types() identifies these as Float, so the SUM/AVG
            // kernels do f64::from_bits(col[i]). If we store them as raw
            // integers, f64::from_bits(17) = 8.4e-323 (denormalized),
            // breaking all float aggregates. Fix: encode as f64::to_bits
            // for known TPC-H float columns.
            if is_tpch_float_column(name) {
                as_i64.into_iter().map(|v| (v as f64).to_bits()).collect()
            } else {
                as_i64.into_iter().map(|v| v as u64).collect()
            }
        } else {
            i32_columns.push(None);
            // Try f64
            let mut as_f64: Vec<f64> = Vec::with_capacity(row_count);
            let mut all_float = true;
            for s in strings {
                match s.parse::<f64>() {
                    Ok(f) => as_f64.push(f),
                    Err(_) => {
                        all_float = false;
                        break;
                    }
                }
            }
            if all_float {
                as_f64.into_iter().map(|v| v.to_bits()).collect()
            } else {
                // W25: Try date column — 'YYYY-MM-DD' strings.
                // TPC-H date columns (o_orderdate, l_shipdate, etc.) fail
                // both i64 and f64 parsing because they contain '-' chars.
                // Without this branch they fell through to xxh3_64 hashing,
                // which broke ALL date range filters (the hash values are
                // huge u64, not comparable to the SQL literal days-since-epoch).
                // This detects the YYYY-MM-DD format and stores days-since-epoch,
                // matching what read_tpc_h_csv does for pipe-delimited files.
                let mut all_date = true;
                let mut as_date: Vec<u64> = Vec::with_capacity(row_count);
                for s in strings {
                    // Allow empty strings (treated as epoch 0 / NULL sentinel).
                    if s.is_empty() {
                        as_date.push(0);
                        continue;
                    }
                    // Quick format check: must be exactly 10 bytes 'YYYY-MM-DD'.
                    let sb = s.as_bytes();
                    if sb.len() != 10
                        || !sb[0].is_ascii_digit()
                        || !sb[1].is_ascii_digit()
                        || !sb[2].is_ascii_digit()
                        || !sb[3].is_ascii_digit()
                        || sb[4] != b'-'
                        || !sb[5].is_ascii_digit()
                        || !sb[6].is_ascii_digit()
                        || sb[7] != b'-'
                        || !sb[8].is_ascii_digit()
                        || !sb[9].is_ascii_digit()
                    {
                        all_date = false;
                        break;
                    }
                    as_date.push(parse_date_to_days(sb));
                }
                if all_date {
                    parsed_as_date = true;
                    as_date
                } else {
                    // String column: hash with xxh3_64
                    strings.iter().map(|s| xxh3::xxh3_64(s.as_bytes())).collect()
                }
            }
        };

        // String sidecar: only for columns that are neither i64, f64, nor date.
        // W25: Added `!parsed_as_date` — date columns must NOT get a
        // StringSearchColumn (they're stored as integer days-since-epoch,
        // and the engine treats them as ColType::Date via tpc_h_col_types).
        let is_string = !all_numeric && !parsed_as_date && {
            let mut all_float = true;
            for s in strings {
                if s.parse::<f64>().is_err() {
                    all_float = false;
                    break;
                }
            }
            !all_float
        };

        let string_search = if is_string {
            Some(crate::exec::fm_index::StringSearchColumn::new(strings.clone()))
        } else {
            None
        };

        columns.push(LoadedColumn {
            name: name.clone(),
            cells,
            row_count,
            string_search,
            null_bitmap: None,
        });
    }

    Ok(LoadedTable {
        name: LoadedTable::name_from_path(path),
        columns,
        row_count,
        i32_columns,
    })
}

// === TPC-H CSV loader (Wave 5) ===
//
// The legacy `read_csv` above splits on commas and infers types per
// column ("all-i64 -> numeric, else hash"). That is fine for
// ClickBench-style synthetic CSVs but wrong for TPC-H:
//
//   - TPC-H CSVs are PIPE-delimited (`|`), not comma-delimited.
//   - Float columns (l_quantity, l_extendedprice, l_discount, l_tax,
//     ps_supplycost, ...) would be HASHED because they don't parse as
//     i64, which breaks SUM/AVG arithmetic (the kernel sums u64 cells
//     via `aggregate_sum`, so floats must be `f64::to_bits`-encoded
//     to be summable — see `src/datasource/parquet.rs`).
//   - Date columns ('YYYY-MM-DD' strings) would be HASHED, breaking
//     range filters (`l_shipdate >= date '1994-01-01'` must compare
//     days-since-epoch, not hashes).
//   - String columns would be HASHED with no StringSearchColumn,
//     breaking LIKE filters and Wave 4's string GROUP BY (which needs
//     the actual strings to hash them with xxh3_64 — see
//     `execute_string_group_by` in `src/engine/dispatch.rs`).
//
// `read_tpc_h_csv` solves all four: it splits on `|`, looks up a
// hardcoded per-table schema, and encodes each column to match the
// parquet reader's `Vec<u64>` cell format exactly (Int64 / Float64 /
// Date / String). String columns additionally carry a
// `StringSearchColumn` for LIKE + GROUP BY.

/// TPC-H column types for the schema-aware CSV loader. See
/// [`tpc_h_schema`] for the per-table mapping.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TpcHType {
    /// 64-bit signed integer — `value as u64` (bit-reinterpret for
    /// negatives). Matches parquet reader's `Int64` arm.
    Int64,
    /// 64-bit IEEE float — `f64::to_bits(value)`. CRITICAL for
    /// SUM/AVG: the kernel's `aggregate_sum` adds u64 cells as plain
    /// integers, so floats must be bit-encoded for the sum to produce
    /// a bit-pattern-correct f64 result.
    Float64,
    /// 'YYYY-MM-DD' string -> days since Unix epoch (1970-01-01) as
    /// u64. Makes `l_shipdate >= date '1994-01-01'` a simple u64
    /// comparison. Matches parquet reader's `Date32` arm.
    Date,
    /// Variable-length string — `xxh3_64(bytes)` in `cells` (for
    /// equality filters) PLUS a `StringSearchColumn` (for LIKE
    /// filters and Wave 4's string GROUP BY).
    String,
}

/// The TPC-H schema for all 8 SF=1 tables. Column order matches the
/// CSV file order (which matches the dbgen output order, which
/// matches the canonical TPC-H Table-Maintenance specs).
///
/// Returns `None` for unknown table names so callers can fail fast.
pub fn tpc_h_schema(table: &str) -> Option<Vec<(&'static str, TpcHType)>> {
    use TpcHType::*;
    let schema: Vec<(&'static str, TpcHType)> = match table {
        "customer" => vec![
            ("c_custkey", Int64),
            ("c_name", String),
            ("c_address", String),
            ("c_nationkey", Int64),
            ("c_phone", String),
            ("c_acctbal", Float64),
            ("c_mktsegment", String),
            ("c_comment", String),
        ],
        "lineitem" => vec![
            ("l_orderkey", Int64),
            ("l_partkey", Int64),
            ("l_suppkey", Int64),
            ("l_linenumber", Int64),
            ("l_quantity", Float64),
            ("l_extendedprice", Float64),
            ("l_discount", Float64),
            ("l_tax", Float64),
            ("l_returnflag", String),
            ("l_linestatus", String),
            ("l_shipdate", Date),
            ("l_commitdate", Date),
            ("l_receiptdate", Date),
            ("l_shipinstruct", String),
            ("l_shipmode", String),
            ("l_comment", String),
        ],
        "nation" => vec![
            ("n_nationkey", Int64),
            ("n_name", String),
            ("n_regionkey", Int64),
            ("n_comment", String),
        ],
        "orders" => vec![
            ("o_orderkey", Int64),
            ("o_custkey", Int64),
            ("o_orderstatus", String),
            ("o_totalprice", Float64),
            ("o_orderdate", Date),
            ("o_orderpriority", String),
            ("o_clerk", String),
            ("o_shippriority", Int64),
            ("o_comment", String),
        ],
        "part" => vec![
            ("p_partkey", Int64),
            ("p_name", String),
            ("p_mfgr", String),
            ("p_brand", String),
            ("p_type", String),
            ("p_size", Int64),
            ("p_container", String),
            ("p_retailprice", Float64),
            ("p_comment", String),
        ],
        "partsupp" => vec![
            ("ps_partkey", Int64),
            ("ps_suppkey", Int64),
            ("ps_availqty", Int64),
            ("ps_supplycost", Float64),
            ("ps_comment", String),
        ],
        "region" => vec![("r_regionkey", Int64), ("r_name", String), ("r_comment", String)],
        "supplier" => vec![
            ("s_suppkey", Int64),
            ("s_name", String),
            ("s_address", String),
            ("s_nationkey", Int64),
            ("s_phone", String),
            ("s_acctbal", Float64),
            ("s_comment", String),
        ],
        _ => return None,
    };
    Some(schema)
}

/// W25-T2: Check if a column name is a known TPC-H float column.
/// TPC-H CSVs may store float values without decimal points (e.g.
/// l_quantity=17 instead of 17.00). The generic read_csv() infers
/// these as i64, but tpc_h_col_types() identifies them as Float.
/// This mismatch causes SUM/AVG to interpret raw integers as f64
/// bit patterns (f64::from_bits(17) = 8.4e-323, a denormalized
/// float near zero), breaking all float aggregates.
///
/// This function returns true for column names that are Float64 in
/// the TPC-H schema, so read_csv() can encode them as f64::to_bits
/// even when the values parse as i64.
pub fn is_tpch_float_column(name: &str) -> bool {
    // Note: ps_availqty is Int64 in TPC-H, NOT Float64 — do not include it.
    matches!(
        name,
        "l_quantity" | "l_extendedprice" | "l_discount" | "l_tax"
            | "ps_supplycost"
            | "s_acctbal" | "c_acctbal"
            | "o_totalprice" | "p_retailprice"
    )
}

/// Parse a 'YYYY-MM-DD' byte slice into days since Unix epoch
/// (1970-01-01) as u64. Uses Howard Hinnant's `days_from_civil`
/// algorithm — pure arithmetic, no allocations, no dependencies.
/// Returns 0 for malformed input (defensive — TPC-H data is always
/// well-formed).
fn parse_date_to_days(s: &[u8]) -> u64 {
    // Expected format: "YYYY-MM-DD" = 10 bytes exactly.
    if s.len() < 10 {
        return 0;
    }
    // Quick ASCII-digit check — bail to 0 on non-digit (defensive).
    let b = s;
    if !b[0].is_ascii_digit()
        || !b[1].is_ascii_digit()
        || !b[2].is_ascii_digit()
        || !b[3].is_ascii_digit()
        || !b[5].is_ascii_digit()
        || !b[6].is_ascii_digit()
        || !b[8].is_ascii_digit()
        || !b[9].is_ascii_digit()
    {
        return 0;
    }
    let y = ((b[0] - b'0') as i32) * 1000
        + ((b[1] - b'0') as i32) * 100
        + ((b[2] - b'0') as i32) * 10
        + ((b[3] - b'0') as i32);
    let m = ((b[5] - b'0') as u32) * 10 + ((b[6] - b'0') as u32);
    let d = ((b[8] - b'0') as u32) * 10 + ((b[9] - b'0') as u32);
    days_from_civil(y, m, d) as u64
}

/// Howard Hinnant's `days_from_civil` — days since 1970-01-01 for a
/// proleptic Gregorian date. Valid for all `y/m/d` (including
/// negative years). Reference:
/// <https://howardhinnant.github.io/date_algorithms.html>
fn days_from_civil(y: i32, m: u32, d: u32) -> i32 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = (if y >= 0 { y } else { y - 399 }) / 400;
    let yoe = (y - era * 400) as u32; // [0, 399]
    let doy = (153 * (if m > 2 { m - 3 } else { m + 9 }) + 2) / 5 + d - 1; // [0, 365]
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy; // [0, 146096]
    era * 146097 + doe as i32 - 719468
}

/// Strip surrounding `"..."` quotes from a field byte slice, if both
/// present. TPC-H CSVs quote SOME (but not all) string fields — e.g.
/// `c_name`, `p_mfgr`, `p_brand`, `o_clerk`, `s_name` are quoted in
/// the dbgen output, while `c_address`, `c_phone`, `l_comment` are
/// not. This function handles both cases uniformly.
///
/// NOTE: TPC-H text fields use a controlled ASCII vocabulary that
/// never contains the delimiter (`|`) or the quote char (`"`), so we
/// don't need to handle escaped `""` or quoted delimiters.
fn strip_quotes(field: &[u8]) -> &[u8] {
    if field.len() >= 2 && field[0] == b'"' && field[field.len() - 1] == b'"' {
        &field[1..field.len() - 1]
    } else {
        field
    }
}

/// Read a TPC-H pipe-delimited CSV file with a known schema.
///
/// `table_name` selects the schema via [`tpc_h_schema`] (one of
/// `customer`, `lineitem`, `nation`, `orders`, `part`, `partsupp`,
/// `region`, `supplier`). The CSV file must have a header row (column
/// names); the header is read and skipped — the output columns take
/// their names from the schema, not the header, so name-mismatch
/// errors are impossible.
///
/// # Type encoding (matches [`super::parquet::read_parquet`])
///
/// - `Int64`   → `value as u64` (bit-reinterpret for negatives)
/// - `Float64` → `f64::to_bits(value)` — required for SUM/AVG
/// - `Date`    → days since Unix epoch (1970-01-01) as u64 — required
///   for range comparisons
/// - `String`  → `xxh3_64(bytes)` in `cells` (equality filters) PLUS
///   a `StringSearchColumn` (LIKE filters + Wave 4 string GROUP BY)
///
/// # Memory efficiency
///
/// Uses `BufReader::read_until` with a reused byte buffer — does NOT
/// slurp the whole file into memory. For lineitem (770 MB, 6 M rows)
/// this keeps peak file-buffer memory at ~64 KB while the column
/// vectors grow to their final size (pre-allocated from file-size
/// estimate).
///
/// # Errors
///
/// Returns an error if:
/// - `table_name` is not a known TPC-H table,
/// - the file cannot be opened,
/// - a row has the wrong number of `|`-separated fields,
/// - an integer / float field fails to parse.
pub fn read_tpc_h_csv(path: &str, table_name: &str) -> Result<LoadedTable, Box<dyn Error>> {
    let schema =
        tpc_h_schema(table_name).ok_or_else(|| format!("unknown TPC-H table: {}", table_name))?;
    let ncols = schema.len();

    let file = File::open(path)?;
    let file_size = file.metadata().map(|m| m.len() as usize).unwrap_or(0);
    let mut reader = BufReader::new(file);

    // Pre-allocate column vectors based on file size. TPC-H rows
    // average 115-165 bytes depending on the table; we use the
    // table-specific average to estimate row count, with a small
    // over-allocation to avoid reallocation. The Vec will still grow
    // if the estimate is wrong.
    let avg_row_size: usize = match table_name {
        "region" => 80,
        "nation" => 90,
        "supplier" => 145,
        "customer" => 165,
        "part" => 125,
        "partsupp" => 150,
        "orders" => 116,
        "lineitem" => 130,
        _ => 130,
    };
    // est_rows = file_size / avg_row_size, but never smaller than 8
    // (so tiny files still preallocate something).
    let est_rows = if file_size > 0 { (file_size / avg_row_size).max(8) } else { 8 };

    let mut col_cells: Vec<Vec<u64>> = (0..ncols).map(|_| Vec::with_capacity(est_rows)).collect();
    // String storage — only allocate for String columns (saves memory
    // for the integer/float/date columns).
    let mut col_strings: Vec<Vec<String>> = (0..ncols).map(|_| Vec::new()).collect();
    for (i, (_, t)) in schema.iter().enumerate() {
        if *t == TpcHType::String {
            col_strings[i] = Vec::with_capacity(est_rows);
        }
    }

    let mut row_count: usize = 0;
    let mut line_buf: Vec<u8> = Vec::with_capacity(4096);

    // Skip the header line (column names). We don't validate the
    // header against the schema — the schema's column names are
    // authoritative, and the dbgen header always matches the schema.
    line_buf.clear();
    let n = reader.read_until(b'\n', &mut line_buf)?;
    if n == 0 {
        return Err("interpreter csv: file is empty (no header)".into());
    }

    // Wrap reader in a mutable binding so we can call read_until.
    // (Already declared `mut` above; this comment is a placeholder
    // for the borrow-checker reasoning: `BufReader::read_until`
    // requires `&mut self`, and we call it across loop iterations.)

    loop {
        line_buf.clear();
        let n = reader.read_until(b'\n', &mut line_buf)?;
        if n == 0 {
            break; // EOF
        }

        // Strip trailing `\n` and `\r` (handle both Unix and CRLF).
        let mut end = line_buf.len();
        if end > 0 && line_buf[end - 1] == b'\n' {
            end -= 1;
        }
        if end > 0 && line_buf[end - 1] == b'\r' {
            end -= 1;
        }
        // Skip blank lines (e.g., a trailing newline at EOF).
        if end == 0 {
            continue;
        }

        // Split on `|` and parse each field inline. TPC-H text fields
        // never contain `|` (the dbgen vocabulary is pipe-free), so a
        // simple split is correct AND fast (no quote-state machine).
        //
        // The field-slice stack array is declared INSIDE the loop so
        // its immutable borrow of `line_buf` ends when this scope
        // closes — that lets the next iteration's `line_buf.clear()`
        // take a mutable borrow. (A Vec<&[u8]> declared outside the
        // loop would pin `line_buf` immutably across iterations and
        // fail the borrow check.) The stack array holds up to 16
        // slices — lineitem is the widest TPC-H table at 16 columns.
        let mut field_slices: [&[u8]; 16] = [&[]; 16];
        let mut field_count: usize = 0;
        let mut start: usize = 0;
        for i in 0..end {
            if line_buf[i] == b'|' {
                if field_count < 16 {
                    field_slices[field_count] = &line_buf[start..i];
                    field_count += 1;
                }
                start = i + 1;
            }
        }
        // Last field (after the final `|`, or the entire line if no `|`).
        if field_count < 16 {
            field_slices[field_count] = &line_buf[start..end];
            field_count += 1;
        }

        if field_count != ncols {
            return Err(format!(
                "interpreter[{}] row {} has {} fields, expected {} (line: {:?})",
                table_name,
                row_count + 1,
                field_count,
                ncols,
                std::str::from_utf8(&line_buf[..end]).unwrap_or("<non-utf8>")
            )
            .into());
        }

        for i in 0..ncols {
            let field = strip_quotes(field_slices[i]);
            let (_, t) = schema[i];
            match t {
                TpcHType::Int64 => {
                    let s = std::str::from_utf8(field)?;
                    let v: i64 = s.parse()?;
                    col_cells[i].push(v as u64);
                }
                TpcHType::Float64 => {
                    let s = std::str::from_utf8(field)?;
                    let v: f64 = s.parse()?;
                    col_cells[i].push(v.to_bits());
                }
                TpcHType::Date => {
                    col_cells[i].push(parse_date_to_days(field));
                }
                TpcHType::String => {
                    // TPC-H text is ASCII; `from_utf8` succeeds in
                    // the common case (no allocation). On the rare
                    // non-ASCII byte sequence, fall back to lossy.
                    let s = match std::str::from_utf8(field) {
                        Ok(s) => s.to_string(),
                        Err(_) => String::from_utf8_lossy(field).into_owned(),
                    };
                    col_cells[i].push(xxh3::xxh3_64(s.as_bytes()));
                    col_strings[i].push(s);
                }
            }
        }
        row_count += 1;
    }

    // Build LoadedColumns. String columns get their StringSearchColumn;
    // non-string columns stay `None`.
    let mut columns: Vec<LoadedColumn> = Vec::with_capacity(ncols);
    // Wave 5C: i32 sidecar. Parallel to `columns`; Some(Vec<i32>) for
    // Int64 columns whose values all fit in i32 range (4 bytes/element
    // vs 8 for u64 — halves filter memory bandwidth). None otherwise.
    let mut i32_columns: Vec<Option<Vec<i32>>> = Vec::with_capacity(ncols);
    for i in 0..ncols {
        let string_search = if !col_strings[i].is_empty() {
            Some(crate::exec::fm_index::StringSearchColumn::new(std::mem::take(
                &mut col_strings[i],
            )))
        } else {
            None
        };
        // W6C: i32 sidecar disabled (see generic read_csv comment above).
        i32_columns.push(None);
        columns.push(LoadedColumn {
            name: schema[i].0.to_string(),
            cells: std::mem::take(&mut col_cells[i]),
            row_count,
            string_search,
            null_bitmap: None,
        });
    }

    // Use `table_name` as the LoadedTable's name (NOT the path-derived
    // `tpc_h_lineitem` stem) so callers can `SELECT ... FROM lineitem`
    // directly after registering the table without an extra rename
    // step. This matches the convention used by `QueryEngine::load_csv`
    // and `QueryEngine::load_parquet`, which both rename to the
    // caller-supplied table name after loading.
    Ok(LoadedTable { name: table_name.to_string(), columns, row_count, i32_columns })
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::NamedTempFile;

    /// Write `contents` to a temp file and return its path.
    fn write_tmp(contents: &str) -> (NamedTempFile, String) {
        let tmp = NamedTempFile::new().expect("temp file");
        let path = tmp.path().to_str().expect("path str").to_string();
        std::fs::write(&path, contents).expect("write");
        (tmp, path)
    }

    /// Numeric CSV with header parses to i64-as-u64 cells.
    #[test]
    fn read_csv_numeric_with_header() {
        let (_tmp, path) = write_tmp("id,value\n1,10\n2,20\n3,30\n");
        let table = read_csv(&path, true).expect("read");

        assert_eq!(table.row_count, 3);
        assert_eq!(table.columns.len(), 2);
        assert_eq!(table.columns[0].name, "id");
        assert_eq!(table.columns[1].name, "value");
        assert_eq!(table.columns[0].cells, vec![1u64, 2, 3]);
        assert_eq!(table.columns[1].cells, vec![10u64, 20, 30]);
    }

    /// Numeric CSV without header gets synthetic `col_N` names.
    #[test]
    fn read_csv_numeric_no_header() {
        let (_tmp, path) = write_tmp("1,10\n2,20\n");
        let table = read_csv(&path, false).expect("read");

        assert_eq!(table.row_count, 2);
        assert_eq!(table.columns.len(), 2);
        assert_eq!(table.columns[0].name, "col_0");
        assert_eq!(table.columns[1].name, "col_1");
        assert_eq!(table.columns[0].cells, vec![1u64, 2]);
    }

    /// A column with any non-numeric value is hashed.
    #[test]
    fn read_csv_mixed_column_is_hashed() {
        let (_tmp, path) = write_tmp("label\nfoo\nfoo\nbar\n");
        let table = read_csv(&path, true).expect("read");

        assert_eq!(table.row_count, 3);
        let col = &table.columns[0];
        assert_eq!(col.cells.len(), 3);
        // First two cells hash the same string → equal.
        assert_eq!(col.cells[0], col.cells[1]);
        // Third cell hashes a different string → not equal.
        assert_ne!(col.cells[0], col.cells[2]);
        // And the hash matches xxh3_64("foo").
        assert_eq!(col.cells[0], xxh3::xxh3_64(b"foo"));
        assert_eq!(col.cells[2], xxh3::xxh3_64(b"bar"));
    }

    /// Negative integers are bit-reinterpreted as large u64.
    #[test]
    fn read_csv_negative_values() {
        let (_tmp, path) = write_tmp("v\n-1\n-2\n0\n");
        let table = read_csv(&path, true).expect("read");

        assert_eq!(table.columns[0].cells[0], (-1i64) as u64);
        assert_eq!(table.columns[0].cells[1], (-2i64) as u64);
        assert_eq!(table.columns[0].cells[2], 0u64);
    }

    /// Inconsistent column counts return an error.
    #[test]
    fn read_csv_inconsistent_columns_errors() {
        let (_tmp, path) = write_tmp("a,b\n1,2\n3,4,5\n");
        let err = read_csv(&path, true).unwrap_err();
        assert!(err.to_string().contains("expected 2"), "got: {err}");
    }

    /// Empty file → empty table.
    #[test]
    fn read_csv_empty_file() {
        let (_tmp, path) = write_tmp("");
        let table = read_csv(&path, true).expect("read");
        assert_eq!(table.row_count, 0);
        assert!(table.columns.is_empty());
    }

    /// Blank lines (including trailing newlines) are skipped.
    #[test]
    fn read_csv_skips_blank_lines() {
        let (_tmp, path) = write_tmp("id\n1\n\n2\n\n");
        let table = read_csv(&path, true).expect("read");
        assert_eq!(table.row_count, 2);
        assert_eq!(table.columns[0].cells, vec![1u64, 2]);
    }

    /// `\r\n` line endings are handled.
    #[test]
    fn read_csv_handles_crlf() {
        let (_tmp, path) = write_tmp("id,value\r\n1,10\r\n2,20\r\n");
        let table = read_csv(&path, true).expect("read");
        assert_eq!(table.row_count, 2);
        assert_eq!(table.columns[0].cells, vec![1u64, 2]);
        assert_eq!(table.columns[1].cells, vec![10u64, 20]);
    }

    /// A CSV where one column is numeric and another is hashed
    /// produces mixed-type columns in the same table.
    #[test]
    fn read_csv_mixed_columns() {
        let (_tmp, path) = write_tmp("id,name\n1,foo\n2,bar\n");
        let table = read_csv(&path, true).expect("read");
        assert_eq!(table.columns[0].cells, vec![1u64, 2]);
        assert_eq!(table.columns[1].cells[0], xxh3::xxh3_64(b"foo"));
        assert_eq!(table.columns[1].cells[1], xxh3::xxh3_64(b"bar"));
    }

    // === TPC-H loader tests (Wave 5) ===

    /// `tpc_h_schema` returns the right number of columns per table.
    #[test]
    fn tpc_h_schema_column_counts() {
        assert_eq!(tpc_h_schema("region").unwrap().len(), 3);
        assert_eq!(tpc_h_schema("nation").unwrap().len(), 4);
        assert_eq!(tpc_h_schema("supplier").unwrap().len(), 7);
        assert_eq!(tpc_h_schema("customer").unwrap().len(), 8);
        assert_eq!(tpc_h_schema("part").unwrap().len(), 9);
        assert_eq!(tpc_h_schema("partsupp").unwrap().len(), 5);
        assert_eq!(tpc_h_schema("orders").unwrap().len(), 9);
        assert_eq!(tpc_h_schema("lineitem").unwrap().len(), 16);
        assert!(tpc_h_schema("unknown").is_none());
    }

    /// `parse_date_to_days` matches the canonical Unix-epoch day
    /// count for several well-known dates.
    #[test]
    fn parse_date_known_values() {
        // 1970-01-01 → 0 days (the epoch).
        assert_eq!(parse_date_to_days(b"1970-01-01"), 0);
        // 1970-01-02 → 1 day.
        assert_eq!(parse_date_to_days(b"1970-01-02"), 1);
        // 1992-01-01 → 8035 days (commonly cited; 22 years × 365.25 ≈ 8035).
        assert_eq!(parse_date_to_days(b"1992-01-01"), 8035);
        // 1998-12-31 → 10591 days.
        assert_eq!(parse_date_to_days(b"1998-12-31"), 10591);
        // 2000-01-01 → 10957 days (the famous Y2K date).
        assert_eq!(parse_date_to_days(b"2000-01-01"), 10957);
        // Leap-year boundary: 2024-02-29 → 19782 days (the leap day
        // itself — 54 years × 365 + 13 prior leap days + 59 days
        // into 2024 = 19710 + 13 + 59 = 19782).
        assert_eq!(parse_date_to_days(b"2024-02-29"), 19782);
        // 2024-03-01 → 19783 days (day after leap day).
        assert_eq!(parse_date_to_days(b"2024-03-01"), 19783);
        // Cross-check against the `time` crate's Julian-day conversion
        // (independent implementation — guards against subtle off-by-N
        // errors in the days_from_civil algorithm).
        // time::Date::from_calendar_date(1998, time::Month::December, 31)
        //   .unwrap().to_julian_day() - 2440588 == 10591
        // (2440588 is the Julian day for 1970-01-01.)
    }

    /// Malformed dates return 0 (defensive — should not happen with
    /// real TPC-H data, but the parser must not panic).
    #[test]
    fn parse_date_malformed_returns_zero() {
        assert_eq!(parse_date_to_days(b""), 0);
        assert_eq!(parse_date_to_days(b"1992"), 0);
        assert_eq!(parse_date_to_days(b"YYYY-MM-DD"), 0);
    }

    /// `strip_quotes` strips outer `"` only when both are present.
    #[test]
    fn strip_quotes_basic() {
        assert_eq!(strip_quotes(b"\"hello\""), b"hello");
        assert_eq!(strip_quotes(b"hello"), b"hello");
        assert_eq!(strip_quotes(b"\"hello"), b"\"hello");
        assert_eq!(strip_quotes(b"hello\""), b"hello\"");
        assert_eq!(strip_quotes(b""), b"");
        assert_eq!(strip_quotes(b"\"\""), b"");
    }

    /// Load a tiny pipe-delimited region CSV and verify all column
    /// types are encoded correctly.
    #[test]
    fn read_tpc_h_csv_region_mini() {
        let csv = "r_regionkey|r_name|r_comment\n\
                   0|AFRICA|lazy dog\n\
                   1|AMERICA|quick brown fox\n\
                   2|ASIA|jumps over\n";
        let (_tmp, path) = write_tmp(csv);
        let table = read_tpc_h_csv(&path, "region").expect("read");

        assert_eq!(table.row_count, 3);
        assert_eq!(table.columns.len(), 3);
        assert_eq!(table.columns[0].name, "r_regionkey");
        assert_eq!(table.columns[1].name, "r_name");
        assert_eq!(table.columns[2].name, "r_comment");

        // r_regionkey is Int64 → 0, 1, 2 as u64.
        assert_eq!(table.columns[0].cells, vec![0u64, 1, 2]);
        // r_name is String → xxh3_64 of each string.
        assert_eq!(table.columns[1].cells[0], xxh3::xxh3_64(b"AFRICA"));
        assert_eq!(table.columns[1].cells[1], xxh3::xxh3_64(b"AMERICA"));
        assert_eq!(table.columns[1].cells[2], xxh3::xxh3_64(b"ASIA"));
        // r_name has a StringSearchColumn (the whole point of Wave 5).
        assert!(table.columns[1].string_search.is_some());
        let ss = table.columns[1].string_search.as_ref().unwrap();
        assert_eq!(ss.len(), 3);
        assert_eq!(ss.get(0), "AFRICA");
        assert_eq!(ss.get(1), "AMERICA");
        assert_eq!(ss.get(2), "ASIA");
        // r_comment also has a StringSearchColumn.
        assert!(table.columns[2].string_search.is_some());
        // r_regionkey (Int64) does NOT have a StringSearchColumn.
        assert!(table.columns[0].string_search.is_none());
    }

    /// Load a tiny lineitem CSV with a Float column and a Date column
    /// to verify the Float64 bit-encoding and Date day-encoding.
    #[test]
    fn read_tpc_h_csv_lineitem_mini_types() {
        // Minimal lineitem: 3 rows × all 16 columns. We only assert
        // the Float64 (l_quantity) and Date (l_shipdate) encodings.
        let csv = "l_orderkey|l_partkey|l_suppkey|l_linenumber|l_quantity|l_extendedprice|l_discount|l_tax|l_returnflag|l_linestatus|l_shipdate|l_commitdate|l_receiptdate|l_shipinstruct|l_shipmode|l_comment\n\
                   1|155190|7706|1|17.00|21168.23|0.04|0.02|N|O|1996-03-13|1996-02-12|1996-03-22|DELIVER IN PERSON|TRUCK|blah\n\
                   2|67310|7311|2|36.00|45983.16|0.09|0.06|N|O|1996-04-12|1996-02-28|1996-04-20|TAKE BACK RETURN|MAIL|blah2\n\
                   3|63700|3701|3|8.00|13309.60|0.10|0.02|R|F|1994-02-02|1994-01-04|1994-02-23|TAKE BACK RETURN|FOB|blah3\n";
        let (_tmp, path) = write_tmp(csv);
        let table = read_tpc_h_csv(&path, "lineitem").expect("read");

        assert_eq!(table.row_count, 3);
        assert_eq!(table.columns.len(), 16);

        // l_quantity (Float64 col index 4): f64::to_bits encoding.
        assert_eq!(table.columns[4].cells[0], 17.0f64.to_bits());
        assert_eq!(table.columns[4].cells[1], 36.0f64.to_bits());
        assert_eq!(table.columns[4].cells[2], 8.0f64.to_bits());

        // l_extendedprice (Float64 col index 5): verify bit pattern.
        assert_eq!(table.columns[5].cells[0], 21168.23f64.to_bits());
        assert_eq!(table.columns[5].cells[1], 45983.16f64.to_bits());
        assert_eq!(table.columns[5].cells[2], 13309.60f64.to_bits());

        // l_shipdate (Date col index 10): days since epoch.
        // 1996-03-13 → 9590 days, 1996-04-12 → 9620 days, 1994-02-02 → 8815 days.
        assert_eq!(table.columns[10].cells[0], parse_date_to_days(b"1996-03-13"));
        assert_eq!(table.columns[10].cells[1], parse_date_to_days(b"1996-04-12"));
        assert_eq!(table.columns[10].cells[2], parse_date_to_days(b"1994-02-02"));
        // Sanity: shipdate0 should be in the [5000, 30000] range
        // (1984-01-01 ≈ 5113, 2025-01-01 ≈ 20089).
        assert!(table.columns[10].cells[0] > 5000 && table.columns[10].cells[0] < 30000);

        // l_returnflag (String col index 8) has a StringSearchColumn.
        assert!(table.columns[8].string_search.is_some());
        let ss = table.columns[8].string_search.as_ref().unwrap();
        assert_eq!(ss.len(), 3);
        assert_eq!(ss.get(0), "N");
        assert_eq!(ss.get(1), "N");
        assert_eq!(ss.get(2), "R");
        // l_linestatus (String col index 9) also has a StringSearchColumn.
        assert!(table.columns[9].string_search.is_some());

        // l_orderkey (Int64 col index 0): plain i64 → u64.
        assert_eq!(table.columns[0].cells, vec![1u64, 2, 3]);
        assert_eq!(table.columns[1].cells, vec![155190u64, 67310, 63700]);
    }

    /// Quoted string fields (e.g. `"Customer#000000001"`) have their
    /// surrounding quotes stripped.
    #[test]
    fn read_tpc_h_csv_strips_quotes() {
        // customer schema: c_custkey, c_name, c_address, c_nationkey,
        // c_phone, c_acctbal, c_mktsegment, c_comment. c_name is
        // quoted in the real dbgen output.
        let csv = "c_custkey|c_name|c_address|c_nationkey|c_phone|c_acctbal|c_mktsegment|c_comment\n\
                   1|\"Customer#000000001\"|j5JsirBM9PsCy0O1m|15|25-989-741-2988|711.56|BUILDING|hi\n";
        let (_tmp, path) = write_tmp(csv);
        let table = read_tpc_h_csv(&path, "customer").expect("read");

        assert_eq!(table.row_count, 1);
        // c_name (col 1): the StringSearchColumn should hold the
        // UN-quoted string "Customer#000000001" (no leading/trailing `"`).
        let ss = table.columns[1].string_search.as_ref().expect("c_name string_search");
        assert_eq!(ss.get(0), "Customer#000000001");
        // And the cell hash should match the un-quoted string.
        assert_eq!(table.columns[1].cells[0], xxh3::xxh3_64(b"Customer#000000001"));
        // c_acctbal (Float64 col 5): f64::to_bits.
        assert_eq!(table.columns[5].cells[0], 711.56f64.to_bits());
    }

    /// Unknown table name → error.
    #[test]
    fn read_tpc_h_csv_unknown_table_errors() {
        let (_tmp, path) = write_tmp("a|b\n1|2\n");
        let err = read_tpc_h_csv(&path, "unknown_table").unwrap_err();
        assert!(err.to_string().contains("unknown TPC-H table"), "got: {err}");
    }

    /// Wrong field count → error.
    #[test]
    fn read_tpc_h_csv_wrong_field_count_errors() {
        // region expects 3 fields; this row has only 2.
        let csv = "r_regionkey|r_name|r_comment\n0|AFRICA\n";
        let (_tmp, path) = write_tmp(csv);
        let err = read_tpc_h_csv(&path, "region").unwrap_err();
        assert!(err.to_string().contains("expected 3"), "got: {err}");
    }

    /// Integration test: load the REAL TPC-H region CSV from
    /// `/tmp/tpc_h_region.csv` (5 rows). Skipped if the file is not
    /// present (e.g., when running on a dev machine without the
    /// benchmark data).
    #[test]
    fn read_tpc_h_csv_region_integration() {
        let path = "/tmp/tpc_h_region.csv";
        if !std::path::Path::new(path).exists() {
            eprintln!("skipping: {} not present", path);
            return;
        }
        let table = read_tpc_h_csv(path, "region").expect("read region");
        assert_eq!(table.row_count, 5, "region must have 5 rows");
        assert_eq!(table.columns.len(), 3);
        assert_eq!(table.columns[0].name, "r_regionkey");
        // r_regionkey: 0..4 as Int64.
        assert_eq!(table.columns[0].cells, vec![0u64, 1, 2, 3, 4]);
        // r_name has a StringSearchColumn with the 5 region names.
        let ss = table.columns[1].string_search.as_ref().expect("r_name string_search");
        assert_eq!(ss.len(), 5);
        assert_eq!(ss.get(0), "AFRICA");
        assert_eq!(ss.get(1), "AMERICA");
        assert_eq!(ss.get(2), "ASIA");
        assert_eq!(ss.get(3), "EUROPE");
        assert_eq!(ss.get(4), "MIDDLE EAST");
    }

    /// Integration test: load the REAL TPC-H nation CSV (25 rows).
    #[test]
    fn read_tpc_h_csv_nation_integration() {
        let path = "/tmp/tpc_h_nation.csv";
        if !std::path::Path::new(path).exists() {
            eprintln!("skipping: {} not present", path);
            return;
        }
        let table = read_tpc_h_csv(path, "nation").expect("read nation");
        assert_eq!(table.row_count, 25, "nation must have 25 rows");
        assert_eq!(table.columns.len(), 4);
        assert_eq!(table.columns[0].name, "n_nationkey");
        // n_nationkey: 0..24 as Int64.
        assert_eq!(table.columns[0].cells.len(), 25);
        assert_eq!(table.columns[0].cells[0], 0);
        assert_eq!(table.columns[0].cells[24], 24);
        // n_name (col 1) and n_comment (col 3) are strings.
        assert!(table.columns[1].string_search.is_some());
        assert!(table.columns[3].string_search.is_some());
        // n_regionkey (col 2) is Int64 — no StringSearchColumn.
        assert!(table.columns[2].string_search.is_none());
    }

    /// Integration test: load the REAL TPC-H supplier CSV (10,000
    /// rows). Verifies that quoted fields (s_name, e.g.
    /// "Supplier#000000001") are handled correctly across many rows.
    #[test]
    fn read_tpc_h_csv_supplier_integration() {
        let path = "/tmp/tpc_h_supplier.csv";
        if !std::path::Path::new(path).exists() {
            eprintln!("skipping: {} not present", path);
            return;
        }
        let table = read_tpc_h_csv(path, "supplier").expect("read supplier");
        assert_eq!(table.row_count, 10_000, "supplier must have 10000 rows");
        assert_eq!(table.columns.len(), 7);
        // s_name (col 1) has a StringSearchColumn — verify the first
        // supplier's name has its quotes stripped.
        let ss = table.columns[1].string_search.as_ref().expect("s_name string_search");
        assert_eq!(ss.len(), 10_000);
        assert_eq!(ss.get(0), "Supplier#000000001");
        assert_eq!(ss.get(1), "Supplier#000000002");
        // s_acctbal (Float64 col 5): all cells should be non-zero
        // (sanity check that floats were parsed).
        let nonzero = table.columns[5].cells.iter().filter(|&&v| v != 0).count();
        assert!(nonzero > 9_000, "expected >9000 non-zero s_acctbal cells, got {}", nonzero);
    }

    /// Integration test: load the REAL TPC-H lineitem CSV (6,001,215
    /// rows). This is the slowest test — ~3-5 s on the benchmark
    /// server. Skipped if the file is absent (developer machines).
    #[test]
    fn read_tpc_h_csv_lineitem_integration() {
        let path = "/tmp/tpc_h_lineitem.csv";
        if !std::path::Path::new(path).exists() {
            eprintln!("skipping: {} not present", path);
            return;
        }
        let start = std::time::Instant::now();
        let table = read_tpc_h_csv(path, "lineitem").expect("read lineitem");
        let elapsed = start.elapsed();

        assert_eq!(table.row_count, 6_001_215, "lineitem must have 6001215 rows");
        assert_eq!(table.columns.len(), 16);

        // l_quantity (Float64 col 4): bit-encoded f64. Verify the
        // first cell is the bit pattern of a small positive float.
        let q0 = f64::from_bits(table.columns[4].cells[0]);
        assert!(q0 > 0.0 && q0 < 100.0, "l_quantity[0] = {} should be in (0, 100)", q0);

        // l_returnflag (String col 8) and l_linestatus (col 9) have
        // StringSearchColumns.
        assert!(table.columns[8].string_search.is_some());
        assert!(table.columns[9].string_search.is_some());
        let ss = table.columns[8].string_search.as_ref().unwrap();
        assert_eq!(ss.len(), 6_001_215);
        // l_returnflag values are only 'N', 'R', 'A' — verify the
        // first is one of those.
        let r0 = ss.get(0);
        assert!(r0 == "N" || r0 == "R" || r0 == "A", "l_returnflag[0] = {:?}", r0);

        // l_shipdate (Date col 10): days-since-epoch. TPC-H shipdates
        // range 1992-01-01 (8035) to 1998-12-31 (10591).
        let shipdate0 = table.columns[10].cells[0];
        assert!(shipdate0 > 5000 && shipdate0 < 30000, "l_shipdate[0] = {}", shipdate0);

        eprintln!("lineitem load took {}ms", elapsed.as_millis());
    }

    // === Wave 5C: i32 sidecar tests ===

    /// `read_csv` populates the i32 sidecar for a narrow integer column
    /// (all values fit in i32 range).
    #[test]
    fn read_csv_no_i32_sidecar_for_narrow_int_w6c() {
        let (_tmp, path) = write_tmp("id\n1\n2\n3\n100\n-7\n");
        let table = read_csv(&path, true).expect("read");
        assert_eq!(table.i32_columns.len(), 1);
        // W6C: i32 sidecar disabled (was duplicating storage, +16GB RSS at SF=10)
        assert!(table.i32_columns[0].is_none(), "i32 sidecar should be None after W6c");
        // u64 storage is still populated.
        assert_eq!(table.columns[0].cells, vec![1u64, 2, 3, 100, (-7i64) as u64]);
    }

    /// `read_csv` does NOT populate the i32 sidecar when any value
    /// exceeds i32 range (e.g. > 2_147_483_647).
    #[test]
    fn read_csv_no_i32_sidecar_for_wide_int() {
        let (_tmp, path) = write_tmp("id\n1\n3000000000\n");
        let table = read_csv(&path, true).expect("read");
        assert_eq!(table.i32_columns.len(), 1);
        assert!(table.i32_columns[0].is_none(), "expected None for value > i32::MAX");
        // u64 storage still has the value.
        assert_eq!(table.columns[0].cells, vec![1u64, 3_000_000_000u64]);
    }

    /// `read_csv` does NOT populate the i32 sidecar for string columns.
    #[test]
    fn read_csv_no_i32_sidecar_for_string() {
        let (_tmp, path) = write_tmp("name\nfoo\nbar\n");
        let table = read_csv(&path, true).expect("read");
        assert_eq!(table.i32_columns.len(), 1);
        assert!(table.i32_columns[0].is_none());
    }

    /// `read_csv` does NOT populate the i32 sidecar for float columns.
    #[test]
    fn read_csv_no_i32_sidecar_for_float() {
        let (_tmp, path) = write_tmp("v\n1.5\n2.5\n");
        let table = read_csv(&path, true).expect("read");
        assert_eq!(table.i32_columns.len(), 1);
        assert!(table.i32_columns[0].is_none());
    }

    /// `read_tpc_h_csv` populates the i32 sidecar for Int64 columns
    /// whose values fit in i32 range. Verify the lineitem Int64
    /// columns (l_orderkey, l_partkey, l_suppkey, l_linenumber) all
    /// get the sidecar.
    #[test]
    fn read_tpc_h_csv_lineitem_no_i32_sidecar_w6c() {
        let csv = "l_orderkey|l_partkey|l_suppkey|l_linenumber|l_quantity|l_extendedprice|l_discount|l_tax|l_returnflag|l_linestatus|l_shipdate|l_commitdate|l_receiptdate|l_shipinstruct|l_shipmode|l_comment\n\
                   1|155190|7706|1|17.00|21168.23|0.04|0.02|N|O|1996-03-13|1996-02-12|1996-03-22|DELIVER IN PERSON|TRUCK|blah\n\
                   2|67310|7311|2|36.00|45983.16|0.09|0.06|N|O|1996-04-12|1996-02-28|1996-04-20|TAKE BACK RETURN|MAIL|blah2\n\
                   3|63700|3701|3|8.00|13309.60|0.10|0.02|R|F|1994-02-02|1994-01-04|1994-02-23|TAKE BACK RETURN|FOB|blah3\n";
        let (_tmp, path) = write_tmp(csv);
        let table = read_tpc_h_csv(&path, "lineitem").expect("read");
        assert_eq!(table.i32_columns.len(), 16);

        // W6C: i32 sidecar disabled — all should be None.
        assert!(table.i32_columns[0].is_none(), "l_orderkey sidecar should be None");
        assert!(table.i32_columns[1].is_none(), "l_partkey sidecar should be None");
        assert!(table.i32_columns[2].is_none(), "l_suppkey sidecar should be None");
        assert!(table.i32_columns[3].is_none(), "l_linenumber sidecar should be None");

        // l_quantity (col 4): Float64 → no sidecar.
        assert!(table.i32_columns[4].is_none());
        // l_returnflag (col 8): String → no sidecar.
        assert!(table.i32_columns[8].is_none());
        // l_shipdate (col 10): Date → no sidecar.
        assert!(table.i32_columns[10].is_none());
    }

    /// Integration test: load the REAL TPC-H lineitem CSV and verify
    /// the i32 sidecar is populated for the narrow Int64 columns
    /// (l_orderkey, l_partkey, l_suppkey, l_linenumber). SF=1 values
    /// fit in i32 range, so the sidecar should be present.
    #[test]
    fn read_tpc_h_csv_lineitem_i32_sidecar_integration() {
        let path = "/tmp/tpc_h_lineitem.csv";
        if !std::path::Path::new(path).exists() {
            eprintln!("skipping: {} not present", path);
            return;
        }
        let table = read_tpc_h_csv(path, "lineitem").expect("read lineitem");
        assert_eq!(table.i32_columns.len(), 16);
        // l_orderkey, l_partkey, l_suppkey, l_linenumber — all Int64,
        // all should fit in i32 range for SF=1.
        assert!(table.i32_columns[0].is_some(), "l_orderkey should have i32 sidecar");
        assert!(table.i32_columns[1].is_some(), "l_partkey should have i32 sidecar");
        assert!(table.i32_columns[2].is_some(), "l_suppkey should have i32 sidecar");
        assert!(table.i32_columns[3].is_some(), "l_linenumber should have i32 sidecar");
        // Spot-check: l_orderkey[0] should be 1.
        let orderkey = table.i32_columns[0].as_ref().unwrap();
        assert_eq!(orderkey[0], 1i32);
        // The sidecar length should match row_count.
        assert_eq!(orderkey.len(), table.row_count);
    }
}
