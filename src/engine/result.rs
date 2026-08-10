//! Query result types — the bridge between the executor and the caller.
//!
//! [`QueryResult`] is what [`crate::engine::QueryEngine::execute`] hands back
//! to the caller: a list of named `Vec<u64>` columns, a row count, and an
//! elapsed-microseconds field captured by the engine.
//!
//! ## Why `Vec<u64>`
//!
//! Every column in turboGP is stored as `Vec<u64>` cells (see
//! [`crate::datasource`]). The result type preserves that shape so callers
//! can re-use the kernel table's scan / aggregate kernels directly on
//! the result without a per-cell conversion layer.
//!
//! ## Scalar helpers
//!
//! [`QueryResult::from_scalar_u64`] and [`QueryResult::from_scalar_f64`] are
//! conveniences for the common `SELECT count(*) FROM t` /
//! `SELECT sum(x) FROM t` patterns where the result is a single cell.
//! Both return `Option` so the caller can handle the empty-result case
//! without panicking.

/// A column in a query result.
///
/// `values.len() == row_count` of the parent [`QueryResult`]. The name
/// matches the select-list item that produced it (for `*`, the column's
/// original name from the catalog; for aggregates, the function name
/// lowercased — e.g. `"count"`, `"sum"`).
///
/// ## String columns (Wave 21)
///
/// For columns that originated from a string-typed source column, the
/// `string_values` field is `Some` and contains the original (non-hashed)
/// string values. The `values` field still contains the xxh3 hashes for
/// backward compatibility. Callers that want to display results should
/// check `string_values` first.
#[derive(Debug, Clone)]
pub struct ResultColumn {
    /// Column name.
    pub name: String,
    /// Cell values, parallel to every other column in the result.
    pub values: Vec<u64>,
    /// Original string values, if this column originated from a string
    /// column in the source table. `None` for numeric/aggregate columns.
    /// When `Some`, `string_values.len() == values.len()`.
    pub string_values: Option<Vec<String>>,
    /// Column type OID for pgwire (Wave 47). 0 = unknown (use heuristic).
    pub type_oid: u32,
    /// Per-row NULL mask (Wave 52 fix). `Some(mask)` where `mask[i] = true`
    /// means row `i` is NULL. `None` means no NULLs in this column.
    /// When `Some`, `null_mask.len() == values.len()`. The pgwire layer
    /// checks this to emit `-1` length (NULL) instead of `"0"` for NULL cells.
    pub null_mask: Option<Vec<bool>>,
}

impl ResultColumn {
    /// Construct a single-cell column from a `u64` value.
    pub fn scalar_u64(name: impl Into<String>, value: u64) -> Self {
        Self {
            name: name.into(),
            values: vec![value],
            string_values: None,
            type_oid: 0,
            null_mask: None,
        }
    }

    /// Construct a single-cell column from an `f64` value, bit-reinterpreted
    /// as `u64` (matching the engine's universal cell format for Float64
    /// columns — see [`crate::datasource`]).
    pub fn scalar_f64(name: impl Into<String>, value: f64) -> Self {
        Self {
            name: name.into(),
            values: vec![value.to_bits()],
            string_values: None,
            type_oid: 0,
            null_mask: None,
        }
    }

    /// Construct a column from u64 values (no string data).
    pub fn from_u64(name: impl Into<String>, values: Vec<u64>) -> Self {
        Self { name: name.into(), values, string_values: None, type_oid: 0, null_mask: None }
    }

    /// Construct a column from string values. The u64 `values` are
    /// computed as xxh3_64 hashes of the strings for backward compatibility.
    pub fn from_strings(name: impl Into<String>, strings: Vec<String>) -> Self {
        use xxhash_rust::xxh3;
        let values: Vec<u64> = strings.iter().map(|s| xxh3::xxh3_64(s.as_bytes())).collect();
        Self {
            name: name.into(),
            values,
            string_values: Some(strings),
            type_oid: 0,
            null_mask: None,
        }
    }

    /// The column's cell values as a slice.
    pub fn as_slice(&self) -> &[u64] {
        &self.values
    }

    /// Returns the string value at `row_idx` if this is a string column.
    pub fn get_string(&self, row_idx: usize) -> Option<&str> {
        self.string_values.as_ref().and_then(|sv| sv.get(row_idx).map(|s| s.as_str()))
    }

    /// Returns true if this column has string data.
    pub fn has_strings(&self) -> bool {
        self.string_values.is_some()
    }
}

/// The result of executing a SQL query.
///
/// A result is a list of [`ResultColumn`]s (all of the same length, the
/// `row_count`) plus an elapsed-microseconds field. Callers consume it
/// either by name (via [`QueryResult::column`]) or as a scalar (via
/// [`QueryResult::from_scalar_u64`] / [`QueryResult::from_scalar_f64`]).
#[derive(Debug, Clone, Default)]
pub struct QueryResult {
    /// The result columns, in select-list order.
    pub columns: Vec<ResultColumn>,
    /// Number of rows. Equal to every column's `values.len()`.
    pub row_count: usize,
    /// Wall-clock execution time in microseconds, captured by
    /// [`crate::engine::QueryEngine::execute`] around the parse → plan →
    /// execute pipeline.
    pub elapsed_us: u64,
}

impl QueryResult {
    /// Construct an empty result (zero columns, zero rows).
    ///
    /// Used as the starting point for builder-style construction in
    /// `execute_select`; the caller pushes columns and then calls
    /// [`QueryResult::finish`] to set `row_count`.
    pub fn empty() -> Self {
        Self { columns: Vec::new(), row_count: 0, elapsed_us: 0 }
    }

    /// Construct a single-cell result from a `u64` value.
    ///
    /// Convenience for `SELECT count(*)` paths where the result is exactly
    /// one row, one column.
    pub fn from_scalar_u64(name: impl Into<String>, value: u64) -> Self {
        Self { columns: vec![ResultColumn::scalar_u64(name, value)], row_count: 1, elapsed_us: 0 }
    }

    /// Construct a single-cell result from an `f64` value.
    ///
    /// Convenience for `SELECT sum(x)` paths. The `f64` is bit-reinterpreted
    /// as `u64` (matching the engine's cell format for Float64 columns).
    pub fn from_scalar_f64(name: impl Into<String>, value: f64) -> Self {
        Self { columns: vec![ResultColumn::scalar_f64(name, value)], row_count: 1, elapsed_us: 0 }
    }

    /// Push a column onto the result. The result's `row_count` is set to
    /// the pushed column's length if it is currently zero; otherwise the
    /// column's length must match the existing `row_count`.
    ///
    /// Returns `Err` if the column length disagrees with the existing
    /// `row_count`, so callers don't silently construct a malformed result.
    pub fn push_column(&mut self, column: ResultColumn) -> Result<(), String> {
        if self.columns.is_empty() {
            self.row_count = column.values.len();
        } else if column.values.len() != self.row_count {
            return Err(format!(
                "column '{}' has {} values but result row_count is {}",
                column.name,
                column.values.len(),
                self.row_count
            ));
        }
        self.columns.push(column);
        Ok(())
    }

    /// Number of columns in the result.
    pub fn column_count(&self) -> usize {
        self.columns.len()
    }

    /// Look up a column by name, returning a slice over its cells.
    ///
    /// Returns `None` if no column with that name exists. The lookup is
    /// `O(ncols)` linear scan — result sets are narrow (typically 1–5
    /// columns), so a `HashMap` would not pay for itself.
    pub fn column(&self, name: &str) -> Option<&[u64]> {
        self.columns.iter().find(|c| c.name == name).map(|c| c.values.as_slice())
    }

    /// Get the first cell of the first column as a `u64`.
    ///
    /// Returns `None` if the result is empty. Used by callers that ran a
    /// `SELECT count(*) FROM t` query and want the count back as a number.
    pub fn scalar_u64(&self) -> Option<u64> {
        self.columns.first().and_then(|c| c.values.first().copied())
    }

    /// Get the first cell of the first column as an `f64` (bit-reinterpreted
    /// from the stored `u64`).
    ///
    /// Returns `None` if the result is empty. Used by callers that ran a
    /// `SELECT sum(x) FROM t` query.
    pub fn scalar_f64(&self) -> Option<f64> {
        self.columns.first().and_then(|c| c.values.first().copied()).map(f64::from_bits)
    }

    /// Pretty-print the result to stdout as a formatted table.
    ///
    /// The format is a simple column-aligned text table:
    ///
    /// ```text
    /// count
    /// ─────
    /// 1000
    /// (1 row in 42 µs)
    /// ```
    ///
    /// For multi-column results, columns are separated by `│`. Cell values
    /// are printed as `u64`; for `f64` aggregates the caller should
    /// bit-reinterpret via [`QueryResult::scalar_f64`] (the getter) before printing.
    pub fn print(&self) {
        if self.columns.is_empty() {
            println!("(empty result, {} rows in {} µs)", self.row_count, self.elapsed_us);
            return;
        }

        // Compute column widths: max(name length, max cell value width).
        let widths: Vec<usize> = self
            .columns
            .iter()
            .map(|c| {
                let name_w = c.name.len();
                let val_w = c.values.iter().map(|v| format!("{v}").len()).max().unwrap_or(0);
                name_w.max(val_w)
            })
            .collect();

        // Header.
        let header: String = self
            .columns
            .iter()
            .zip(&widths)
            .map(|(c, w)| format!("{:<w$}", c.name, w = w))
            .collect::<Vec<_>>()
            .join(" │ ");
        println!("{header}");

        // Separator.
        let sep: String = widths.iter().map(|w| "─".repeat(*w)).collect::<Vec<_>>().join("─┼─");
        println!("{sep}");

        // Rows.
        for row_idx in 0..self.row_count {
            let row: String = self
                .columns
                .iter()
                .zip(&widths)
                .map(|(c, w)| {
                    let v = c.values.get(row_idx).copied().unwrap_or(0);
                    format!("{:<w$}", v, w = w)
                })
                .collect::<Vec<_>>()
                .join(" │ ");
            println!("{row}");
        }

        // Footer.
        println!(
            "({} row{} in {} µs)",
            self.row_count,
            if self.row_count == 1 { "" } else { "s" },
            self.elapsed_us
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn scalar_u64_round_trip() {
        let r = QueryResult::from_scalar_u64("count", 42u64);
        assert_eq!(r.row_count, 1);
        assert_eq!(r.column_count(), 1);
        assert_eq!(r.scalar_u64(), Some(42));
        assert_eq!(r.column("count"), Some(&[42u64][..]));
        assert_eq!(r.column("missing"), None);
    }

    #[test]
    fn scalar_f64_round_trip() {
        let r = QueryResult::from_scalar_f64("sum", 42.75f64);
        assert_eq!(r.row_count, 1);
        let s = r.scalar_f64().expect("scalar");
        assert!((s - 42.75).abs() < 1e-12, "got {s}");
    }

    #[test]
    fn empty_result_prints_summary() {
        // Just verify it doesn't panic on an empty result.
        let r = QueryResult::empty();
        r.print();
        assert_eq!(r.row_count, 0);
        assert_eq!(r.column_count(), 0);
        assert_eq!(r.scalar_u64(), None);
    }

    #[test]
    fn push_column_sets_row_count() {
        let mut r = QueryResult::empty();
        r.push_column(ResultColumn {
            name: "x".into(),
            values: vec![1, 2, 3],
            string_values: None,
            type_oid: 0,
            null_mask: None,
        })
        .expect("push");
        assert_eq!(r.row_count, 3);
        r.push_column(ResultColumn {
            name: "y".into(),
            values: vec![10, 20, 30],
            string_values: None,
            type_oid: 0,
            null_mask: None,
        })
        .expect("push");
        assert_eq!(r.row_count, 3);
        assert_eq!(r.column_count(), 2);
    }

    #[test]
    fn push_column_rejects_length_mismatch() {
        let mut r = QueryResult::empty();
        r.push_column(ResultColumn {
            name: "x".into(),
            values: vec![1, 2, 3],
            string_values: None,
            type_oid: 0,
            null_mask: None,
        })
        .expect("push");
        let err = r
            .push_column(ResultColumn {
                name: "y".into(),
                values: vec![10, 20],
                string_values: None,
                type_oid: 0,
                null_mask: None,
            })
            .unwrap_err();
        assert!(err.contains("row_count"), "got: {err}");
    }

    #[test]
    fn column_lookup_by_name() {
        let mut r = QueryResult::empty();
        r.push_column(ResultColumn {
            name: "id".into(),
            values: vec![1, 2, 3],
            string_values: None,
            type_oid: 0,
            null_mask: None,
        })
        .expect("push");
        r.push_column(ResultColumn {
            name: "v".into(),
            values: vec![10, 20, 30],
            string_values: None,
            type_oid: 0,
            null_mask: None,
        })
        .expect("push");
        assert_eq!(r.column("id"), Some(&[1u64, 2, 3][..]));
        assert_eq!(r.column("v"), Some(&[10u64, 20, 30][..]));
        assert_eq!(r.column("missing"), None);
    }

    #[test]
    fn scalar_returns_none_for_empty_result() {
        let r = QueryResult::empty();
        assert_eq!(r.scalar_u64(), None);
        assert_eq!(r.scalar_f64(), None);
    }

    #[test]
    fn print_does_not_panic_on_multi_column() {
        let mut r = QueryResult::empty();
        r.push_column(ResultColumn {
            name: "id".into(),
            values: vec![1, 2, 3],
            string_values: None,
            type_oid: 0,
            null_mask: None,
        })
        .expect("push");
        r.push_column(ResultColumn {
            name: "v".into(),
            values: vec![10, 20, 30],
            string_values: None,
            type_oid: 0,
            null_mask: None,
        })
        .expect("push");
        r.print();
        // No assertion — the test just verifies print doesn't panic.
    }

    #[test]
    fn result_column_scalar_constructors() {
        let c = ResultColumn::scalar_u64("count", 7u64);
        assert_eq!(c.name, "count");
        assert_eq!(c.values, vec![7]);
        assert_eq!(c.as_slice(), &[7u64][..]);

        let c = ResultColumn::scalar_f64("sum", 2.5f64);
        assert_eq!(c.values, vec![2.5f64.to_bits()]);
    }

    #[test]
    fn default_is_empty() {
        let r = QueryResult::default();
        assert_eq!(r.row_count, 0);
        assert_eq!(r.column_count(), 0);
        assert_eq!(r.elapsed_us, 0);
    }
}
