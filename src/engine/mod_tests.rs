#[cfg(test)]
mod tests {
    use super::*;
    use crate::datasource::parquet::{LoadedColumn, LoadedTable};
    use crate::datasource::Table as DataSourceTable;
    use arrow::array::Int64Array;
    use arrow::datatypes::{DataType, Field, Schema};
    use arrow::record_batch::RecordBatch;
    use std::sync::Arc as ArrowArc;
    use tempfile::NamedTempFile;

    /// Build a `Table` with two columns: `id` (0..n) and `x` (cycling 0..7).
    fn make_table(n: usize) -> DataSourceTable {
        let ids: Vec<u64> = (0..n).map(|i| i as u64).collect();
        let xs: Vec<u64> = (0..n).map(|i| (i % 7) as u64).collect();
        DataSourceTable::from_loaded(LoadedTable {
            name: "t".into(),
            columns: vec![
                LoadedColumn {
                    name: "id".into(),
                    cells: ids,
                    row_count: n,
                    string_search: None,
                    null_bitmap: None,
                },
                LoadedColumn {
                    name: "x".into(),
                    cells: xs,
                    row_count: n,
                    string_search: None,
                    null_bitmap: None,
                },
            ],
            row_count: n,
        })
    }

    /// Build a `Table` with a single integer-encoded column `v`.
    fn make_int_table(values: &[u64]) -> DataSourceTable {
        let n = values.len();
        DataSourceTable::from_loaded(LoadedTable {
            name: "ft".into(),
            columns: vec![LoadedColumn {
                name: "v".into(),
                cells: values.to_vec(),
                row_count: n,
                string_search: None,
                null_bitmap: None,
            }],
            row_count: n,
        })
    }

    // -----------------------------------------------------------------
    // DoD tests (the 9 cases from the Wave 20 task brief)
    // -----------------------------------------------------------------

    /// DoD 1: `SELECT count(*) FROM t` returns the table's row count.
    #[test]
    fn dod_count_star_returns_row_count() {
        let mut engine = QueryEngine::in_memory();
        engine.register_table(make_table(1000));
        let r = engine.execute("SELECT count(*) FROM t").expect("query");
        assert_eq!(r.scalar_u64(), Some(1000));
    }

    /// DoD 2: `SELECT count(*) FROM t WHERE x = 42` returns the right count.
    #[test]
    fn dod_count_star_with_where() {
        let mut engine = QueryEngine::in_memory();
        // Make a table where x = 42 appears exactly 7 times.
        let mut xs: Vec<u64> = (0..1000).map(|i| (i % 7) as u64).collect();
        // Make some entries equal to 42.
        for i in 0..7 {
            xs[i * 100] = 42;
        }
        let table = DataSourceTable::from_loaded(LoadedTable {
            name: "t".into(),
            columns: vec![
                LoadedColumn {
                    name: "id".into(),
                    cells: (0..1000).map(|i| i as u64).collect(),
                    row_count: 1000,
                    string_search: None,
                    null_bitmap: None,
                },
                LoadedColumn {
                    name: "x".into(),
                    cells: xs,
                    row_count: 1000,
                    string_search: None,
                    null_bitmap: None,
                },
            ],
            row_count: 1000,
        });
        engine.register_table(table);

        let r = engine.execute("SELECT count(*) FROM t WHERE x = 42").expect("query");
        assert_eq!(r.scalar_u64(), Some(7));
    }

    /// DoD 3: `SELECT sum(col) FROM t` returns the right sum.
    #[test]
    fn dod_sum_returns_correct_sum() {
        let mut engine = QueryEngine::in_memory();
        engine.register_table(make_table(1000));
        let r = engine.execute("SELECT sum(id) FROM t").expect("query");
        let s = r.scalar_f64().expect("scalar");
        assert!((s - 499_500.0).abs() < 1e-3, "got {s}");
    }

    /// DoD 4: `SELECT * FROM t WHERE id = 5` returns the matching row.
    #[test]
    fn dod_select_star_with_where() {
        let mut engine = QueryEngine::in_memory();
        engine.register_table(make_table(1000));
        let r = engine.execute("SELECT * FROM t WHERE id = 5").expect("query");
        assert_eq!(r.row_count, 1);
        assert_eq!(r.column("id"), Some(&[5u64][..]));
        assert_eq!(r.column("x"), Some(&[5u64][..])); // 5 % 7 = 5
    }

    /// DoD 5: APPROXIMATE extension parses and runs.
    #[test]
    fn dod_count_distinct_with_approximate() {
        let mut engine = QueryEngine::in_memory();
        engine.register_table(make_table(1000));
        let r = engine
            .execute("SELECT count(DISTINCT x) APPROXIMATE WITHIN 0.05 CONFIDENCE 0.95 FROM t")
            .expect("query");
        assert_eq!(r.scalar_u64(), Some(7));
    }

    /// DoD 6: TIER extension parses and runs.
    #[test]
    fn dod_count_star_with_tier_l3() {
        let mut engine = QueryEngine::in_memory();
        engine.register_table(make_table(1000));
        let r = engine.execute("SELECT count(*) FROM t TIER L3").expect("query");
        assert_eq!(r.scalar_u64(), Some(1000));
    }

    /// DoD 7: Invalid SQL returns `Error::Parse`.
    #[test]
    fn dod_invalid_sql_returns_parse_error() {
        let mut engine = QueryEngine::in_memory();
        let r = engine.execute("SELECT FROM WHERE");
        assert!(matches!(r, Err(Error::Parse(_))), "got {r:?}");
    }

    /// DoD 8: Non-existent table returns `Error::NotFound`.
    #[test]
    fn dod_non_existent_table_returns_not_found() {
        let mut engine = QueryEngine::in_memory();
        let r = engine.execute("SELECT count(*) FROM missing");
        assert!(matches!(r, Err(Error::NotFound(_))), "got {r:?}");
    }

    /// DoD 9: Load a Parquet file, query it.
    #[test]
    fn dod_load_parquet_and_query() {
        // Build a small Parquet file with one Int64 column `id` of 100 rows.
        let tmp = NamedTempFile::new().expect("temp file");
        let path = tmp.path().to_str().expect("path str").to_string();
        let ids: Vec<i64> = (0..100).collect();
        let arr = ArrowArc::new(Int64Array::from(ids));
        let schema = ArrowArc::new(Schema::new(vec![Field::new("id", DataType::Int64, false)]));
        let batch = RecordBatch::try_new(schema, vec![arr]).expect("batch");
        crate::datasource::parquet::write_parquet_for_test(&path, &batch).expect("write");

        let mut engine = QueryEngine::in_memory();
        let n = engine.load_parquet(&path, "loaded").expect("load");
        assert_eq!(n, 100);

        let r = engine.execute("SELECT count(*) FROM loaded").expect("query");
        assert_eq!(r.scalar_u64(), Some(100));

        let r = engine.execute("SELECT sum(id) FROM loaded").expect("query");
        let s = r.scalar_f64().expect("scalar");
        assert!((s - 4950.0).abs() < 1e-3, "got {s}"); // 0+1+...+99 = 4950
    }

    // -----------------------------------------------------------------
    // Additional integration tests
    // -----------------------------------------------------------------

    /// Load a CSV file and query it.
    #[test]
    fn load_csv_and_query() {
        let tmp = NamedTempFile::new().expect("temp file");
        let path = tmp.path().to_str().expect("path str").to_string();
        std::fs::write(&path, "id,value\n1,10\n2,20\n3,30\n4,40\n5,50\n").expect("write");

        let mut engine = QueryEngine::in_memory();
        let n = engine.load_csv(&path, "csvt", true).expect("load");
        assert_eq!(n, 5);

        let r = engine.execute("SELECT count(*) FROM csvt").expect("query");
        assert_eq!(r.scalar_u64(), Some(5));

        let r = engine.execute("SELECT count(*) FROM csvt WHERE value = 30").expect("query");
        assert_eq!(r.scalar_u64(), Some(1));

        let r = engine.execute("SELECT sum(value) FROM csvt").expect("query");
        let s = r.scalar_f64().expect("scalar");
        assert!((s - 150.0).abs() < 1e-9, "got {s}"); // 10+20+30+40+50 = 150

        let r = engine.execute("SELECT * FROM csvt WHERE id = 3").expect("query");
        assert_eq!(r.row_count, 1);
        assert_eq!(r.column("id"), Some(&[3u64][..]));
        assert_eq!(r.column("value"), Some(&[30u64][..]));
    }

    /// Sum of an integer-encoded column through the engine API.
    #[test]
    fn engine_sum_integer_column() {
        let mut engine = QueryEngine::in_memory();
        // Integer-encoded column: 1, 2, 3, 4 → sum = 10.
        engine.register_table(make_int_table(&[1, 2, 3, 4]));
        let r = engine.execute("SELECT sum(v) FROM ft").expect("query");
        let s = r.scalar_f64().expect("scalar");
        assert!((s - 10.0).abs() < 1e-9, "got {s}");
    }

    /// The elapsed_us field is populated after `execute`.
    #[test]
    fn execute_populates_elapsed_us() {
        let mut engine = QueryEngine::in_memory();
        engine.register_table(make_table(100));
        let r = engine.execute("SELECT count(*) FROM t").expect("query");
        // elapsed_us should be non-negative (and almost certainly > 0,
        // but we don't assert that to avoid flakes on very fast machines).
        assert!(r.elapsed_us < 1_000_000, "elapsed_us unreasonably large: {}", r.elapsed_us);
    }

    /// Re-registering a table replaces the old one.
    #[test]
    fn register_table_overwrites() {
        let mut engine = QueryEngine::in_memory();
        engine.register_table(make_table(100));
        engine.register_table(make_table(200));
        let r = engine.execute("SELECT count(*) FROM t").expect("query");
        assert_eq!(r.scalar_u64(), Some(200));
    }

    /// `with_cost_model` constructs an engine with a non-default cost model.
    #[test]
    fn with_cost_model_constructs_engine() {
        let cm = CostModel { cpu_freq_hz: 4.0e9, simd_lanes: 16, ..CostModel::default() };
        let mut engine = QueryEngine::with_cost_model(cm);
        assert_eq!(engine.cost_model().cpu_freq_hz, 4.0e9);
        assert_eq!(engine.cost_model().simd_lanes, 16);
    }

    /// `QueryEngine::default()` is equivalent to `new()`.
    /// The catalog contains the internal `__dummy__` table (used for
    /// FROM-less SELECTs), so it's not strictly empty — but it has no
    /// user-registered tables.
    #[test]
    fn default_is_empty() {
        let mut engine = QueryEngine::default();
        // The __dummy__ table is always present.
        assert_eq!(engine.catalog().len(), 1);
        // But no user tables.
        let names: Vec<&str> =
            engine.catalog().table_names().into_iter().filter(|n| *n != "__dummy__").collect();
        assert!(names.is_empty());
    }

    /// Accessors return the right types.
    #[test]
    fn accessors_work() {
        let mut engine = QueryEngine::in_memory();
        let _cat: &Catalog = engine.catalog();
        let _kt: &KernelTable = engine.kernel_table();
        let _cm: &CostModel = engine.cost_model();
    }

    /// A query against a table with zero rows returns 0 for count(*).
    #[test]
    fn count_star_on_empty_table_returns_zero() {
        let mut engine = QueryEngine::in_memory();
        engine.register_table(make_table(0));
        let r = engine.execute("SELECT count(*) FROM t").expect("query");
        assert_eq!(r.scalar_u64(), Some(0));
    }

    /// A sum against a table with zero rows returns 0.0.
    #[test]
    fn sum_on_empty_table_returns_zero() {
        let mut engine = QueryEngine::in_memory();
        engine.register_table(make_table(0));
        let r = engine.execute("SELECT sum(id) FROM t").expect("query");
        let s = r.scalar_f64().expect("scalar");
        assert!(s.abs() < 1e-9, "got {s}");
    }

    /// Print does not panic on a real result.
    #[test]
    fn print_does_not_panic() {
        let mut engine = QueryEngine::in_memory();
        engine.register_table(make_table(10));
        let r = engine.execute("SELECT * FROM t").expect("query");
        r.print();
        // No assertion — the test just verifies print doesn't panic.
    }

    /// Extensions other than TIER/APPROXIMATE are accepted (no-ops).
    #[test]
    fn other_extensions_accepted() {
        let mut engine = QueryEngine::in_memory();
        engine.register_table(make_table(100));
        let r = engine
            .execute("SELECT count(*) FROM t USING HYPERLOGLOG MEMORY BUDGET 1048576 ENERGY BUDGET 100 JOULES CONSISTENCY STRONG")
            .expect("query");
        assert_eq!(r.scalar_u64(), Some(100));
    }

    /// Loading a Parquet file under a custom name works.
    #[test]
    fn load_parquet_under_custom_name() {
        let tmp = NamedTempFile::new().expect("temp file");
        let path = tmp.path().to_str().expect("path str").to_string();
        let arr = ArrowArc::new(Int64Array::from(vec![1i64, 2, 3]));
        let schema = ArrowArc::new(Schema::new(vec![Field::new("id", DataType::Int64, false)]));
        let batch = RecordBatch::try_new(schema, vec![arr]).expect("batch");
        crate::datasource::parquet::write_parquet_for_test(&path, &batch).expect("write");

        let mut engine = QueryEngine::in_memory();
        let n = engine.load_parquet(&path, "custom_name").expect("load");
        assert_eq!(n, 3);

        // The table is registered under "custom_name", not the file stem.
        let r = engine.execute("SELECT count(*) FROM custom_name").expect("query");
        assert_eq!(r.scalar_u64(), Some(3));

        // The file stem is NOT registered.
        let r = engine.execute("SELECT count(*) FROM tempfile");
        assert!(matches!(r, Err(Error::NotFound(_))), "got {r:?}");
    }

    /// Parquet Int64 column round-trips through a load + count + sum.
    #[test]
    fn parquet_int_column_count_and_sum() {
        let tmp = NamedTempFile::new().expect("temp file");
        let path = tmp.path().to_str().expect("path str").to_string();
        // Int64 column 1..=5 → integer-encoded as 1u64..=5.
        let arr = ArrowArc::new(Int64Array::from(vec![1i64, 2, 3, 4, 5]));
        let schema = ArrowArc::new(Schema::new(vec![Field::new("v", DataType::Int64, false)]));
        let batch = RecordBatch::try_new(schema, vec![arr]).expect("batch");
        crate::datasource::parquet::write_parquet_for_test(&path, &batch).expect("write");

        let mut engine = QueryEngine::in_memory();
        engine.load_parquet(&path, "ft").expect("load");

        // Count.
        let r = engine.execute("SELECT count(*) FROM ft").expect("query");
        assert_eq!(r.scalar_u64(), Some(5));

        // Sum (integer-encoded: 1+2+3+4+5 = 15).
        let r = engine.execute("SELECT sum(v) FROM ft").expect("query");
        let s = r.scalar_f64().expect("scalar");
        assert!((s - 15.0).abs() < 1e-9, "got {s}");

        // Count distinct.
        let r = engine.execute("SELECT count(DISTINCT v) FROM ft").expect("query");
        assert_eq!(r.scalar_u64(), Some(5));

        // SELECT * with filter.
        let r = engine.execute("SELECT * FROM ft WHERE v = 3").expect("query");
        assert_eq!(r.row_count, 1);
        assert_eq!(r.column("v"), Some(&[3u64][..]));
    }
}
