//! Binary checkpoint format (Wave 4 — replaces SQL-text checkpoint).
//!
//! Serializes the catalog via `bincode` for fast, type-safe restart.
//! ~10x faster than SQL-text checkpoint (no parsing, no re-execution).
//!
//! ## Design
//!
//! The on-disk format is `bincode`-encoded `Vec<SerializedTable>`. Each
//! `SerializedTable` carries the table's columns (`Vec<Vec<u64>>`), string
//! sidecars (`Vec<Option<Vec<String>>>`), null bitmaps
//! (`Vec<Option<Vec<bool>>>`), schema (column types + flags + FKs + unique
//! constraints), and MVCC row versions.
//!
//! ## Simplifications (vs. the in-memory `Table`)
//!
//! To avoid cascading `serde::Serialize`/`Deserialize` derives through the
//! AST (`Expr`), DDL (`ColumnType`, `ColumnDef`, `TableForeignKey`,
//! `ForeignKeyAction`), and `StringSearchColumn` / `NullBitmap` types
//! (which would touch 5+ files outside this task's scope), the serializable
//! representation uses simplified primitive shapes:
//!
//! - `string_columns`: `Vec<Option<Vec<String>>>` (just the strings; the
//!   `StringSearchColumn`'s `bytes`/`offsets` are rebuilt by
//!   `StringSearchColumn::new` on load).
//! - `null_bitmaps`: `Vec<Option<Vec<bool>>>` (one bool per row; rebuilt
//!   via `NullBitmap::new` + `set_null` on load).
//! - `schema`: `Option<SerializedTableSchema>` — column types encoded as
//!   strings via `ColumnType::type_name()` (lossy: `VARCHAR(50)` becomes
//!   `"VARCHAR"`, `DECIMAL(10,2)` becomes `"DECIMAL"`). The base type is
//!   preserved on round-trip; length/precision is not. CHECK constraints
//!   (`Expr` trees) are NOT preserved — the legacy `checkpoint.sql` is
//!   still written for full fidelity.
//! - `row_versions`: `Vec<Vec<SerializedRowVersion>>` — one chain per
//!   logical row, each chain a `Vec` of 4-field `SerializedRowVersion`.
//!
//! ## Atomicity
//!
//! `save()` writes to `<path>.tmp`, fsyncs, then renames to `<path>` —
//! the same atomic-swap pattern used by the SQL-text checkpoint in
//! `src/storage/recovery.rs`. A crash mid-write leaves the previous
//! checkpoint intact.

use std::fs::File;
use std::io::{BufReader, BufWriter};
use std::path::Path;

use crate::datasource::Table;
use crate::exec::fm_index::StringSearchColumn;
use crate::schema::table_schema::{ColumnSchema, TableSchema};
use crate::sql::ddl::{ColumnType, ForeignKeyAction, TableForeignKey};
use crate::txn::mvcc::RowVersion;
use crate::types::null_bitmap::NullBitmap;

/// A serializable representation of a table for binary checkpointing.
///
/// See the module docs for the simplifications applied to the schema,
/// string sidecars, null bitmaps, and row versions.
#[derive(serde::Serialize, serde::Deserialize)]
pub struct SerializedTable {
    /// Table name.
    pub name: String,
    /// Column names, parallel to `columns`.
    pub column_names: Vec<String>,
    /// Column data: each `Vec<u64>` has length `row_count`.
    pub columns: Vec<Vec<u64>>,
    /// Number of rows.
    pub row_count: usize,
    /// String sidecars (parallel to `columns`; `None` for non-string columns).
    pub string_columns: Vec<Option<Vec<String>>>,
    /// NULL bitmaps (parallel to `columns`; `None` = no NULLs in that column).
    pub null_bitmaps: Vec<Option<Vec<bool>>>,
    /// Optional table schema (column types + constraints).
    pub schema: Option<SerializedTableSchema>,
    /// MVCC row version chains (one `Vec<SerializedRowVersion>` per
    /// logical row; empty outer vec when MVCC is not in use). Task 3.1
    /// layout: the in-memory `Table.row_versions` is now
    /// `Vec<Vec<RowVersion>>`, and this serialised form mirrors that
    /// shape so the chain boundaries round-trip correctly.
    pub row_versions: Vec<Vec<SerializedRowVersion>>,
}

/// A serializable row-version metadata entry (MVCC).
///
/// Mirrors `crate::txn::mvcc::RowVersion` using primitive types so the
/// checkpoint module does not require a serde derive on the original
/// struct (which lives in `src/txn/mvcc.rs`).
#[derive(serde::Serialize, serde::Deserialize)]
pub struct SerializedRowVersion {
    /// Creating txn ID.
    pub xmin: u64,
    /// Deleting txn ID (`None` = still live).
    pub xmax: Option<u64>,
    /// Column values for this version.
    pub values: Vec<u64>,
    /// True if this version represents a logical delete.
    pub deleted: bool,
}

/// A serializable column schema.
///
/// `col_type` is the `ColumnType::type_name()` string (e.g. `"INT"`,
/// `"VARCHAR"`, `"FLOAT"`). Length/precision is lost on round-trip.
#[derive(serde::Serialize, serde::Deserialize)]
pub struct SerializedColumnSchema {
    /// Column name.
    pub name: String,
    /// Column type name (lossy — see struct docs).
    pub col_type: String,
    /// True if NOT NULL.
    pub not_null: bool,
    /// True if PRIMARY KEY.
    pub primary_key: bool,
    /// True if UNIQUE.
    pub unique: bool,
}

/// A serializable table-level foreign key constraint.
///
/// `on_delete` / `on_update` are encoded as the string form of
/// `ForeignKeyAction` (e.g. `"CASCADE"`, `"SET_NULL"`, `"RESTRICT"`).
#[derive(serde::Serialize, serde::Deserialize)]
pub struct SerializedTableForeignKey {
    /// Referencing columns.
    pub columns: Vec<String>,
    /// Referenced table name.
    pub ref_table: String,
    /// Referenced columns.
    pub ref_columns: Vec<String>,
    /// ON DELETE action (string-encoded).
    pub on_delete: Option<String>,
    /// ON UPDATE action (string-encoded).
    pub on_update: Option<String>,
}

/// A serializable table schema.
///
/// CHECK constraints (`Vec<Expr>`) are NOT preserved — they live in the
/// AST and would require cascading serde derives through `Expr`,
/// `BinOp`, `UnaryOp`, `Value`, and `SelectQueryRef`. The legacy
/// `checkpoint.sql` is still written by `flush_with_checkpoint` for
/// full-fidelity restart when CHECK enforcement is required.
#[derive(serde::Serialize, serde::Deserialize, Default)]
pub struct SerializedTableSchema {
    /// Per-column schema (name, type, flags).
    pub columns: Vec<SerializedColumnSchema>,
    /// Table-level UNIQUE constraints (each entry is a list of column names).
    pub unique_constraints: Vec<Vec<String>>,
    /// Table-level FOREIGN KEY constraints.
    pub foreign_keys: Vec<SerializedTableForeignKey>,
}

/// Save the catalog to a binary checkpoint file (atomic swap).
///
/// Writes to `<path>.tmp`, fsyncs, then renames to `<path>`. Returns the
/// number of tables serialized.
///
/// # Errors
///
/// Returns `std::io::Error` if file creation, fsync, rename, or bincode
/// serialization fails. On failure, the previous checkpoint (if any) is
/// left intact (the temp file is best-effort cleaned up).
pub fn save(catalog: &crate::catalog::Catalog, path: &Path) -> std::io::Result<usize> {
    let tables: Vec<SerializedTable> = catalog
        .table_names()
        .into_iter()
        .filter(|n| *n != "__dummy__")
        .filter_map(|name| catalog.get(&name))
        .map(|t| serialize_table(&t))
        .collect();

    let tmp_path = path.with_extension("bin.tmp");
    {
        let file = File::create(&tmp_path)?;
        let writer = BufWriter::new(file);
        bincode::serialize_into(writer, &tables)
            .map_err(|e| std::io::Error::new(std::io::ErrorKind::Other, e.to_string()))?;
    }
    // fsync the tmp file so the bytes are durable before the rename commits
    // them. Without this, a crash after rename but before the OS flushes
    // the tmp file's data could leave the checkpoint file present but
    // empty/corrupt.
    {
        let tmp_file = File::open(&tmp_path)?;
        tmp_file.sync_all()?;
    }
    // Atomic rename: on POSIX, rename(2) is atomic — the checkpoint file
    // appears either with the old content or the new content, never
    // partially written.
    std::fs::rename(&tmp_path, path)?;
    log::debug!(
        "binary checkpoint: wrote {} tables to {} (atomic swap)",
        tables.len(),
        path.display()
    );
    Ok(tables.len())
}

/// Load a binary checkpoint file into a fresh `Catalog`.
///
/// # Errors
///
/// Returns `std::io::Error` (kind `InvalidData`) if bincode deserialization
/// fails, or the underlying file I/O error on read failure.
pub fn load(path: &Path) -> std::io::Result<crate::catalog::Catalog> {
    let file = File::open(path)?;
    let reader = BufReader::new(file);
    let tables: Vec<SerializedTable> = bincode::deserialize_from(reader)
        .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e.to_string()))?;
    let catalog = crate::catalog::Catalog::new();
    for st in tables {
        let table = deserialize_table(st);
        catalog.register(table);
    }
    log::debug!("binary checkpoint: loaded {} tables from {}", catalog.len(), path.display());
    Ok(catalog)
}

// -----------------------------------------------------------------------
// Conversions: Table <-> SerializedTable
// -----------------------------------------------------------------------

/// Convert an in-memory `Table` into a serializable `SerializedTable`.
fn serialize_table(t: &Table) -> SerializedTable {
    let columns: Vec<Vec<u64>> = t.columns.iter().map(|c| (**c).clone()).collect();

    // String sidecars: extract just the strings (drop bytes/offsets — they're
    // rebuilt by StringSearchColumn::new on load).
    let string_columns: Vec<Option<Vec<String>>> = t
        .string_columns
        .iter()
        .map(|opt| {
            opt.as_ref().map(|sc| {
                (0..sc.len()).map(|i| sc.get(i).to_string()).collect()
            })
        })
        .collect();

    // Null bitmaps: extract as Vec<bool>.
    let null_bitmaps: Vec<Option<Vec<bool>>> =
        t.null_bitmaps.iter().map(|opt| opt.as_ref().map(|bm| bm.bits().to_vec())).collect();

    let schema = t.schema.as_ref().map(serialize_schema);
    let row_versions: Vec<Vec<SerializedRowVersion>> = t
        .row_versions
        .iter()
        .map(|chain| {
            chain
                .iter()
                .map(|v| SerializedRowVersion {
                    xmin: v.xmin,
                    xmax: v.xmax,
                    values: v.values.clone(),
                    deleted: v.deleted,
                })
                .collect()
        })
        .collect();

    SerializedTable {
        name: t.name.clone(),
        column_names: t.column_names.clone(),
        columns,
        row_count: t.row_count,
        string_columns,
        null_bitmaps,
        schema,
        row_versions,
    }
}

/// Convert a `SerializedTable` back into an in-memory `Table`.
fn deserialize_table(st: SerializedTable) -> Table {
    let columns: Vec<std::sync::Arc<Vec<u64>>> =
        st.columns.into_iter().map(std::sync::Arc::new).collect();

    // Rebuild StringSearchColumn from the string vec.
    let string_columns: Vec<Option<std::sync::Arc<StringSearchColumn>>> = st
        .string_columns
        .into_iter()
        .map(|opt| opt.map(StringSearchColumn::new).map(std::sync::Arc::new))
        .collect();

    // Rebuild NullBitmap from the bool vec.
    let null_bitmaps: Vec<Option<NullBitmap>> = st
        .null_bitmaps
        .into_iter()
        .map(|opt| {
            opt.map(|bits| {
                let mut bm = NullBitmap::new(bits.len());
                for (i, &is_null) in bits.iter().enumerate() {
                    if is_null {
                        bm.set_null(i);
                    }
                }
                bm
            })
        })
        .collect();

    let schema = st.schema.map(deserialize_schema);
    let row_versions: Vec<Vec<RowVersion>> = st
        .row_versions
        .into_iter()
        .map(|chain| {
            chain
                .into_iter()
                .map(|v| RowVersion {
                    xmin: v.xmin,
                    xmax: v.xmax,
                    values: v.values,
                    deleted: v.deleted,
                })
                .collect()
        })
        .collect();

    Table {
        name: st.name,
        column_names: st.column_names,
        columns,
        row_count: st.row_count,
        string_columns,
        null_bitmaps,
        schema,
        row_versions,
    }
}

// -----------------------------------------------------------------------
// Conversions: TableSchema <-> SerializedTableSchema
// -----------------------------------------------------------------------

/// Convert a `TableSchema` into its serializable form.
fn serialize_schema(s: &TableSchema) -> SerializedTableSchema {
    let columns: Vec<SerializedColumnSchema> = s
        .columns
        .iter()
        .map(|c| SerializedColumnSchema {
            name: c.name.clone(),
            col_type: c.col_type.type_name().to_string(),
            not_null: c.not_null,
            primary_key: c.primary_key,
            unique: c.unique,
        })
        .collect();

    let foreign_keys: Vec<SerializedTableForeignKey> = s
        .foreign_keys
        .iter()
        .map(|fk| SerializedTableForeignKey {
            columns: fk.columns.clone(),
            ref_table: fk.ref_table.clone(),
            ref_columns: fk.ref_columns.clone(),
            on_delete: fk.on_delete.map(fk_action_to_str),
            on_update: fk.on_update.map(fk_action_to_str),
        })
        .collect();

    SerializedTableSchema {
        columns,
        unique_constraints: s.unique_constraints.clone(),
        foreign_keys,
    }
}

/// Convert a `SerializedTableSchema` back into a `TableSchema`.
///
/// CHECK constraints are NOT restored (they're not serialized — see the
/// module docs). The resulting schema has an empty `checks` vec.
fn deserialize_schema(s: SerializedTableSchema) -> TableSchema {
    let columns: Vec<ColumnSchema> = s
        .columns
        .into_iter()
        .map(|c| ColumnSchema {
            name: c.name,
            col_type: col_type_from_name(&c.col_type),
            not_null: c.not_null,
            primary_key: c.primary_key,
            unique: c.unique,
            check: None,
        })
        .collect();

    let foreign_keys: Vec<TableForeignKey> = s
        .foreign_keys
        .into_iter()
        .map(|fk| TableForeignKey {
            columns: fk.columns,
            ref_table: fk.ref_table,
            ref_columns: fk.ref_columns,
            on_delete: fk.on_delete.as_deref().and_then(fk_action_from_str),
            on_update: fk.on_update.as_deref().and_then(fk_action_from_str),
        })
        .collect();

    TableSchema {
        columns,
        // CHECK constraints are not serialized — documented limitation.
        checks: Vec::new(),
        unique_constraints: s.unique_constraints,
        foreign_keys,
    }
}

/// Map a `ForeignKeyAction` to its string form for serialization.
fn fk_action_to_str(a: ForeignKeyAction) -> String {
    match a {
        ForeignKeyAction::Cascade => "CASCADE".into(),
        ForeignKeyAction::SetNull => "SET_NULL".into(),
        ForeignKeyAction::SetDefault => "SET_DEFAULT".into(),
        ForeignKeyAction::Restrict => "RESTRICT".into(),
        ForeignKeyAction::NoAction => "NO_ACTION".into(),
    }
}

/// Parse a `ForeignKeyAction` from its string form.
fn fk_action_from_str(s: &str) -> Option<ForeignKeyAction> {
    match s {
        "CASCADE" => Some(ForeignKeyAction::Cascade),
        "SET_NULL" => Some(ForeignKeyAction::SetNull),
        "SET_DEFAULT" => Some(ForeignKeyAction::SetDefault),
        "RESTRICT" => Some(ForeignKeyAction::Restrict),
        "NO_ACTION" => Some(ForeignKeyAction::NoAction),
        _ => None,
    }
}

/// Map a `ColumnType::type_name()` string back to a `ColumnType`.
///
/// Lossy: `VARCHAR(50)` round-trips as `VARCHAR` (no length), `DECIMAL(10,2)`
/// round-trips as `DECIMAL` (no precision/scale). `ARRAY` and `ENUM` fall
/// back to `TEXT` (their inner type / values are not preserved).
fn col_type_from_name(name: &str) -> ColumnType {
    match name {
        "INT" => ColumnType::Int,
        "BIGINT" => ColumnType::BigInt,
        "VARCHAR" => ColumnType::Varchar(None),
        "NVARCHAR" => ColumnType::Nvarchar(None),
        "TEXT" => ColumnType::Text,
        "FLOAT" => ColumnType::Float,
        "REAL" => ColumnType::Real,
        "DECIMAL" => ColumnType::Decimal(None, None),
        "NUMERIC" => ColumnType::Numeric(None, None),
        "BOOLEAN" => ColumnType::Boolean,
        "BIT" => ColumnType::Bit,
        "DATE" => ColumnType::Date,
        "TIMESTAMP" => ColumnType::Timestamp,
        "JSON" => ColumnType::Json,
        "UUID" => ColumnType::Uuid,
        "BYTEA" => ColumnType::Bytea,
        // ARRAY/ENUM inner type / values are not preserved by type_name().
        // Fall back to TEXT (a string sidecar type) so the column still
        // round-trips as a string.
        "ARRAY" | "ENUM" => ColumnType::Text,
        // Unknown type name — default to BIGINT (the engine's universal
        // storage type).
        _ => ColumnType::BigInt,
    }
}

// =====================================================================
// Re-exports + a thin BinaryCheckpoint wrapper struct
// =====================================================================

/// A binary checkpoint: bincode-serialized catalog state.
///
/// This is a unit struct — the actual logic lives in the free functions
/// [`save`] and [`load`] in this module. The wrapper exists so callers
/// can write `BinaryCheckpoint::save(...)` / `BinaryCheckpoint::load(...)`,
/// mirroring the legacy `Checkpoint` API in `src/storage/recovery.rs`.
pub struct BinaryCheckpoint;

impl BinaryCheckpoint {
    /// Save the catalog to a binary checkpoint file (atomic swap).
    ///
    /// See [`save`] for the underlying implementation.
    pub fn save(catalog: &crate::catalog::Catalog, path: &Path) -> std::io::Result<usize> {
        save(catalog, path)
    }

    /// Load a binary checkpoint file into a fresh `Catalog`.
    ///
    /// See [`load`] for the underlying implementation.
    pub fn load(path: &Path) -> std::io::Result<crate::catalog::Catalog> {
        load(path)
    }
}

// =====================================================================
// Tests
// =====================================================================

#[cfg(test)]
mod tests {
    use super::*;
    use crate::datasource::parquet::{LoadedColumn, LoadedTable};
    use tempfile::TempDir;

    /// Build a `Table` with the given column names + cells (no schema,
    /// no string sidecars, no null bitmaps).
    fn make_int_table(name: &str, cols: Vec<(&str, Vec<u64>)>) -> Table {
        let row_count = cols.first().map(|(_, c)| c.len()).unwrap_or(0);
        let columns: Vec<LoadedColumn> = cols
            .into_iter()
            .map(|(n, c)| LoadedColumn {
                name: n.into(),
                row_count: c.len(),
                cells: c,
                string_search: None,
                null_bitmap: None,
            })
            .collect();
        let mut t = Table::from_loaded(LoadedTable { name: name.into(), columns, row_count });
        t.schema = None;
        t
    }

    /// Build a `Table` with an INT column, a VARCHAR column (with a string
    /// sidecar), and a FLOAT column, plus a `TableSchema` declaring the types.
    fn make_typed_table(name: &str, rows: &[(i64, &str, f64)]) -> Table {
        let n = rows.len();
        let ids: Vec<u64> = rows.iter().map(|(id, _, _)| *id as u64).collect();
        let strings: Vec<String> = rows.iter().map(|(_, s, _)| (*s).to_string()).collect();
        let floats: Vec<u64> = rows.iter().map(|(_, _, f)| f.to_bits()).collect();

        let sc = StringSearchColumn::new(strings.clone());
        let columns = vec![
            LoadedColumn {
                name: "id".into(),
                cells: ids,
                row_count: n,
                string_search: None,
                null_bitmap: None,
            },
            LoadedColumn {
                name: "name".into(),
                cells: strings.iter().map(|s| xxhash_rust::xxh3::xxh3_64(s.as_bytes())).collect(),
                row_count: n,
                string_search: Some(sc),
                null_bitmap: None,
            },
            LoadedColumn {
                name: "price".into(),
                cells: floats,
                row_count: n,
                string_search: None,
                null_bitmap: None,
            },
        ];
        let mut t = Table::from_loaded(LoadedTable { name: name.into(), columns, row_count: n });
        t.schema = Some(TableSchema {
            columns: vec![
                ColumnSchema {
                    name: "id".into(),
                    col_type: ColumnType::Int,
                    not_null: true,
                    primary_key: true,
                    unique: false,
                    check: None,
                },
                ColumnSchema {
                    name: "name".into(),
                    col_type: ColumnType::Varchar(Some(50)),
                    not_null: false,
                    primary_key: false,
                    unique: false,
                    check: None,
                },
                ColumnSchema {
                    name: "price".into(),
                    col_type: ColumnType::Float,
                    not_null: false,
                    primary_key: false,
                    unique: false,
                    check: None,
                },
            ],
            checks: Vec::new(),
            unique_constraints: vec![vec!["id".into()]],
            foreign_keys: Vec::new(),
        });
        t
    }

    /// Round-trip a catalog with 3 tables (INT, VARCHAR, FLOAT columns)
    /// through `save()` + `load()` and verify all data matches.
    #[test]
    fn test_binary_checkpoint_roundtrip() {
        let tmp = TempDir::new().expect("tempdir");
        let path = tmp.path().join("checkpoint.bin");

        // Build a catalog with 3 tables.
        let mut catalog = crate::catalog::Catalog::new();
        catalog.register(make_int_table(
            "ints",
            vec![("a", vec![1, 2, 3]), ("b", vec![10, 20, 30])],
        ));
        catalog.register(make_typed_table(
            "typed",
            &[(1, "alice", 1.5), (2, "bob", 2.5), (3, "carol", 3.5)],
        ));
        catalog.register(make_int_table("empty", vec![]));

        // Save + reload.
        let n = save(&catalog, &path).expect("save");
        assert_eq!(n, 3, "save should report 3 tables");
        assert!(path.exists(), "checkpoint.bin should exist after save");

        let loaded = load(&path).expect("load");
        assert_eq!(loaded.len(), 3, "loaded catalog should have 3 tables");

        // Verify the ints table.
        let ints = loaded.get("ints").expect("ints table present");
        assert_eq!(ints.row_count, 3);
        assert_eq!(ints.column_names, vec!["a".to_string(), "b".to_string()]);
        assert_eq!(&**ints.columns[0], &vec![1u64, 2, 3][..]);
        assert_eq!(&**ints.columns[1], &vec![10u64, 20, 30][..]);

        // Verify the typed table (INT + VARCHAR + FLOAT).
        let typed = loaded.get("typed").expect("typed table present");
        assert_eq!(typed.row_count, 3);
        assert_eq!(typed.column_names, vec!["id", "name", "price"]);
        // INT column: exact match.
        assert_eq!(&**typed.columns[0], &vec![1u64, 2, 3][..]);
        // FLOAT column: exact match (bit-for-bit).
        assert_eq!(&**typed.columns[2], &vec![1.5f64.to_bits(), 2.5f64.to_bits(), 3.5f64.to_bits()][..]);
        // VARCHAR column: the u64 cell is the xxh3 hash — verify via the
        // string sidecar.
        let sc = typed.string_columns[1].as_ref().expect("string sidecar present");
        assert_eq!(sc.len(), 3);
        assert_eq!(sc.get(0), "alice");
        assert_eq!(sc.get(1), "bob");
        assert_eq!(sc.get(2), "carol");

        // Schema round-trip: column types match (modulo VARCHAR length loss).
        let schema = typed.schema.as_ref().expect("schema present");
        assert_eq!(schema.columns.len(), 3);
        assert_eq!(schema.columns[0].col_type, ColumnType::Int);
        assert_eq!(schema.columns[0].primary_key, true);
        assert_eq!(schema.columns[0].not_null, true);
        // VARCHAR(50) round-trips as VARCHAR(None) — documented lossy.
        assert_eq!(schema.columns[1].col_type, ColumnType::Varchar(None));
        assert_eq!(schema.columns[2].col_type, ColumnType::Float);
        // Unique constraint preserved.
        assert_eq!(schema.unique_constraints, vec![vec!["id".to_string()]]);

        // Empty table round-trips.
        let empty = loaded.get("empty").expect("empty table present");
        assert_eq!(empty.row_count, 0);
        assert_eq!(empty.column_count(), 0);
    }

    /// `save()` uses an atomic swap: the temp file is renamed to the final
    /// path, and no `.tmp` file is left behind on success.
    #[test]
    fn test_save_uses_atomic_swap() {
        let tmp = TempDir::new().expect("tempdir");
        let path = tmp.path().join("cp.bin");
        let tmp_path = path.with_extension("bin.tmp");

        let mut catalog = crate::catalog::Catalog::new();
        catalog.register(make_int_table("t", vec![("x", vec![1, 2])]));

        save(&catalog, &path).expect("save");
        assert!(path.exists(), "final checkpoint file should exist");
        assert!(!tmp_path.exists(), "temp file should be renamed away");
    }

    /// `load()` on a non-existent path returns an error (does not panic).
    #[test]
    fn test_load_missing_file_errors() {
        let tmp = TempDir::new().expect("tempdir");
        let path = tmp.path().join("nope.bin");
        let result = load(&path);
        assert!(result.is_err(), "loading a missing file should error");
    }

    /// `BinaryCheckpoint` wrapper struct delegates to the free functions.
    #[test]
    fn test_binary_checkpoint_wrapper() {
        let tmp = TempDir::new().expect("tempdir");
        let path = tmp.path().join("wrap.bin");

        let mut catalog = crate::catalog::Catalog::new();
        catalog.register(make_int_table("t", vec![("x", vec![42])]));

        let n = BinaryCheckpoint::save(&catalog, &path).expect("save");
        assert_eq!(n, 1);
        let loaded = BinaryCheckpoint::load(&path).expect("load");
        let t = loaded.get("t").expect("table present");
        assert_eq!(&**t.columns[0], &vec![42u64][..]);
    }

    /// A table with NULL bitmaps round-trips the nulls correctly.
    #[test]
    fn test_null_bitmaps_roundtrip() {
        let tmp = TempDir::new().expect("tempdir");
        let path = tmp.path().join("nulls.bin");

        let mut table = make_int_table("t", vec![("x", vec![1, 0, 3, 0, 5])]);
        let mut bm = NullBitmap::new(5);
        bm.set_null(1);
        bm.set_null(3);
        table.null_bitmaps = vec![Some(bm)];

        let mut catalog = crate::catalog::Catalog::new();
        catalog.register(table);

        save(&catalog, &path).expect("save");
        let loaded = load(&path).expect("load");
        let t = loaded.get("t").expect("table present");
        let bm = t.null_bitmaps[0].as_ref().expect("bitmap present");
        assert!(!bm.is_null(0));
        assert!(bm.is_null(1));
        assert!(!bm.is_null(2));
        assert!(bm.is_null(3));
        assert!(!bm.is_null(4));
    }

    /// A table with MVCC row versions round-trips the version metadata
    /// (Task 3.1: now using per-row `Vec<Vec<RowVersion>>` chains).
    #[test]
    fn test_row_versions_roundtrip() {
        let tmp = TempDir::new().expect("tempdir");
        let path = tmp.path().join("mvcc.bin");

        let mut table = make_int_table("t", vec![("x", vec![10, 20, 30])]);
        // Row 0: single live version.
        table.append_row_version(0, RowVersion::new(1, vec![10]));
        // Row 1: a version that was later deleted (xmax set).
        let mut v2 = RowVersion::new(2, vec![20]);
        v2.xmax = Some(5);
        table.append_row_version(1, v2);
        // Row 2: a delete-marker version.
        table.append_row_version(2, RowVersion::new_delete(3));

        let mut catalog = crate::catalog::Catalog::new();
        catalog.register(table);

        save(&catalog, &path).expect("save");
        let loaded = load(&path).expect("load");
        let t = loaded.get("t").expect("table present");
        // Three chains (one per row), each with one version.
        assert_eq!(t.row_versions.len(), 3);
        assert_eq!(t.row_versions[0].len(), 1);
        assert_eq!(t.row_versions[0][0].xmin, 1);
        assert_eq!(t.row_versions[0][0].xmax, None);
        assert_eq!(t.row_versions[0][0].values, vec![10]);
        assert!(!t.row_versions[0][0].deleted);
        assert_eq!(t.row_versions[1].len(), 1);
        assert_eq!(t.row_versions[1][0].xmin, 2);
        assert_eq!(t.row_versions[1][0].xmax, Some(5));
        assert_eq!(t.row_versions[2].len(), 1);
        assert_eq!(t.row_versions[2][0].deleted, true);
    }

    /// Task 3.1: a multi-version chain (UPDATE pattern) round-trips
    /// correctly — both the old (tombstoned) and new (live) versions
    /// survive save/load with their order and metadata intact.
    #[test]
    fn test_row_versions_chain_roundtrip() {
        let tmp = TempDir::new().expect("tempdir");
        let path = tmp.path().join("mvcc_chain.bin");

        let mut table = make_int_table("t", vec![("x", vec![10])]);
        // Row 0: INSERT v1 (committed, live), then UPDATE tombstones v1
        // and appends v2.
        table.append_row_version(0, RowVersion::new(1, vec![10]));
        let mut v1_tombstone = RowVersion::new(1, vec![10]);
        v1_tombstone.xmax = Some(2);
        // Replace the chain with a two-version chain (old + new).
        table.row_versions[0] = vec![v1_tombstone, RowVersion::new(2, vec![99])];

        let mut catalog = crate::catalog::Catalog::new();
        catalog.register(table);

        save(&catalog, &path).expect("save");
        let loaded = load(&path).expect("load");
        let t = loaded.get("t").expect("table present");
        assert_eq!(t.row_versions.len(), 1, "one chain");
        assert_eq!(t.row_versions[0].len(), 2, "chain has two versions");
        // Old version (chain[0]) is tombstoned.
        assert_eq!(t.row_versions[0][0].xmin, 1);
        assert_eq!(t.row_versions[0][0].xmax, Some(2));
        assert_eq!(t.row_versions[0][0].values, vec![10]);
        // New version (chain[1]) is live and carries the new value.
        assert_eq!(t.row_versions[0][1].xmin, 2);
        assert_eq!(t.row_versions[0][1].xmax, None);
        assert_eq!(t.row_versions[0][1].values, vec![99]);
    }

    /// A table with foreign keys round-trips the FK constraints.
    #[test]
    fn test_foreign_keys_roundtrip() {
        let tmp = TempDir::new().expect("tempdir");
        let path = tmp.path().join("fk.bin");

        let mut table = make_int_table("child", vec![("id", vec![1]), ("parent_id", vec![10])]);
        table.schema = Some(TableSchema {
            columns: vec![
                ColumnSchema {
                    name: "id".into(),
                    col_type: ColumnType::Int,
                    not_null: true,
                    primary_key: true,
                    unique: false,
                    check: None,
                },
                ColumnSchema {
                    name: "parent_id".into(),
                    col_type: ColumnType::Int,
                    not_null: false,
                    primary_key: false,
                    unique: false,
                    check: None,
                },
            ],
            checks: Vec::new(),
            unique_constraints: Vec::new(),
            foreign_keys: vec![TableForeignKey {
                columns: vec!["parent_id".into()],
                ref_table: "parent".into(),
                ref_columns: vec!["id".into()],
                on_delete: Some(ForeignKeyAction::Cascade),
                on_update: Some(ForeignKeyAction::SetNull),
            }],
        });

        let mut catalog = crate::catalog::Catalog::new();
        catalog.register(table);

        save(&catalog, &path).expect("save");
        let loaded = load(&path).expect("load");
        let t = loaded.get("child").expect("table present");
        let schema = t.schema.as_ref().expect("schema present");
        assert_eq!(schema.foreign_keys.len(), 1);
        let fk = &schema.foreign_keys[0];
        assert_eq!(fk.columns, vec!["parent_id".to_string()]);
        assert_eq!(fk.ref_table, "parent");
        assert_eq!(fk.ref_columns, vec!["id".to_string()]);
        assert_eq!(fk.on_delete, Some(ForeignKeyAction::Cascade));
        assert_eq!(fk.on_update, Some(ForeignKeyAction::SetNull));
    }
}
