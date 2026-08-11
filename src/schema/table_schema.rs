//! # Table schema (Wave 36).
//!
//! Preserves column type information from CREATE TABLE through to
//! query execution and result formatting. Previously, execute_ddl
//! discarded all type info — every column became Vec<u64>.

use crate::sql::ddl::ColumnType;

/// Schema for a single column: name + type + constraints.
#[derive(Debug, Clone)]
pub struct ColumnSchema {
    pub name: String,
    pub col_type: ColumnType,
    pub not_null: bool,
    pub primary_key: bool,
    /// True if UNIQUE was specified at the column level (Task 3.2).
    /// Enforced at INSERT and UPDATE time by `execute_insert` /
    /// `execute_update` in `engine/dml.rs`.
    pub unique: bool,
    /// Optional CHECK constraint expression at the column level
    /// (Task 3.5). Evaluated against the row's u64 cells by
    /// `eval_check_expr` in `engine/helpers.rs`.
    pub check: Option<crate::sql::ast::Expr>,
}

/// Schema for a table: column schemas in order.
#[derive(Debug, Clone, Default)]
pub struct TableSchema {
    pub columns: Vec<ColumnSchema>,
    /// Table-level CHECK constraints (Task 3.5). Each is an expression
    /// that must evaluate to TRUE (or UNKNOWN) for every row.
    /// Populated by `from_create_table`; `from_ddl` leaves this empty
    /// (it doesn't have access to the `CreateTable` struct).
    pub checks: Vec<crate::sql::ast::Expr>,
    /// Table-level (multi-column) UNIQUE constraints (Task 3.2). Each
    /// entry is a list of column names whose combination must be unique
    /// across rows. Populated by `from_create_table`; `from_ddl` leaves
    /// this empty.
    pub unique_constraints: Vec<Vec<String>>,
    /// Foreign key constraints (Task 3.4). Populated by `from_create_table`
    /// from both table-level `FOREIGN KEY (...) REFERENCES ...` clauses
    /// and column-level `col TYPE REFERENCES other(col)` shorthand.
    /// Enforced at INSERT/UPDATE/DELETE time by `engine/dml.rs`.
    pub foreign_keys: Vec<crate::sql::ddl::TableForeignKey>,
}

impl TableSchema {
    pub fn new() -> Self {
        Self {
            columns: Vec::new(),
            checks: Vec::new(),
            unique_constraints: Vec::new(),
            foreign_keys: Vec::new(),
        }
    }

    pub fn from_ddl(cols: &[crate::sql::ddl::ColumnDef]) -> Self {
        Self {
            columns: cols
                .iter()
                .map(|c| ColumnSchema {
                    name: c.name.clone(),
                    col_type: c.col_type.clone(),
                    not_null: c.not_null,
                    primary_key: c.primary_key,
                    unique: c.unique,
                    check: c.check.clone(),
                })
                .collect(),
            // `from_ddl` only sees the column list; table-level
            // constraints (CHECK / multi-column UNIQUE / FOREIGN KEY)
            // are not available here. Callers that need them should use
            // `from_create_table`.
            //
            // Column-level `REFERENCES` (which IS available here) is
            // converted into `TableForeignKey` entries so that ALTER
            // TABLE ADD COLUMN with REFERENCES also gets FK enforcement.
            checks: Vec::new(),
            unique_constraints: Vec::new(),
            foreign_keys: column_fks_from_ddl(cols),
        }
    }

    /// Build a `TableSchema` from a full `CreateTable` AST node,
    /// preserving table-level CHECK and multi-column UNIQUE constraints
    /// (Task 3.2 + 3.5).
    ///
    /// This is the preferred constructor when the full DDL is available.
    /// `from_ddl` is kept for backward compatibility (it only populates
    /// column-level constraints).
    pub fn from_create_table(ct: &crate::sql::ddl::CreateTable) -> Self {
        Self {
            columns: ct
                .columns
                .iter()
                .map(|c| ColumnSchema {
                    name: c.name.clone(),
                    col_type: c.col_type.clone(),
                    not_null: c.not_null,
                    primary_key: c.primary_key,
                    unique: c.unique,
                    check: c.check.clone(),
                })
                .collect(),
            checks: ct.checks.clone(),
            unique_constraints: ct.unique_constraints.clone(),
            // Combine table-level FOREIGN KEY clauses with column-level
            // `col TYPE REFERENCES other(col)` shorthand. Both produce
            // `TableForeignKey` entries consumed by `engine/dml.rs`.
            foreign_keys: {
                let mut fks = ct.foreign_keys.clone();
                fks.extend(column_fks_from_ddl(&ct.columns));
                fks
            },
        }
    }

    /// Get the type of a column by name.
    pub fn col_type(&self, name: &str) -> Option<&ColumnType> {
        self.columns.iter().find(|c| c.name == name).map(|c| &c.col_type)
    }

    /// Get the type of a column by index.
    pub fn col_type_at(&self, idx: usize) -> Option<&ColumnType> {
        self.columns.get(idx).map(|c| &c.col_type)
    }

    /// Check if a column is a string type (VARCHAR, NVARCHAR, TEXT, JSON,
    /// ARRAY, BYTEA — all stored as string sidecars).
    pub fn is_string(&self, idx: usize) -> bool {
        match self.col_type_at(idx) {
            Some(ColumnType::Varchar(_))
            | Some(ColumnType::Nvarchar(_))
            | Some(ColumnType::Text)
            | Some(ColumnType::Json)
            | Some(ColumnType::Array(_))
            | Some(ColumnType::Bytea) => true,
            _ => false,
        }
    }

    /// Check if a column is a float type (FLOAT, REAL, DECIMAL, NUMERIC).
    pub fn is_float(&self, idx: usize) -> bool {
        match self.col_type_at(idx) {
            Some(ColumnType::Float)
            | Some(ColumnType::Real)
            | Some(ColumnType::Decimal(_, _))
            | Some(ColumnType::Numeric(_, _)) => true,
            _ => false,
        }
    }

    /// Format a u64 cell value for display based on the column type.
    pub fn format_cell(&self, idx: usize, value: u64) -> String {
        match self.col_type_at(idx) {
            Some(ColumnType::Float)
            | Some(ColumnType::Real)
            | Some(ColumnType::Decimal(_, _))
            | Some(ColumnType::Numeric(_, _)) => {
                let f = f64::from_bits(value);
                if f.is_finite() {
                    format!("{f}")
                } else {
                    value.to_string()
                }
            }
            Some(ColumnType::Boolean) | Some(ColumnType::Bit) => {
                if value != 0 {
                    "true".into()
                } else {
                    "false".into()
                }
            }
            _ => value.to_string(),
        }
    }

    /// Get the Postgres type OID for a column.
    pub fn pg_type_oid(&self, idx: usize) -> u32 {
        match self.col_type_at(idx) {
            Some(ColumnType::Int) | Some(ColumnType::SmallInt) | Some(ColumnType::TinyInt) => 23, // int4
            Some(ColumnType::BigInt) => 20, // int8
            Some(ColumnType::Float)
            | Some(ColumnType::Decimal(_, _))
            | Some(ColumnType::Numeric(_, _)) => 701, // float8
            Some(ColumnType::Real) => 700,  // float4
            Some(ColumnType::Boolean) | Some(ColumnType::Bit) => 16, // bool
            Some(ColumnType::Date) => 1082, // date
            Some(ColumnType::Timestamp) => 1114, // timestamp
            Some(ColumnType::Varchar(_))
            | Some(ColumnType::Nvarchar(_))
            | Some(ColumnType::Text) => 25, // text
            Some(ColumnType::Json) => 114,  // json
            Some(ColumnType::Array(_)) => 1007, // _text (text array)
            Some(ColumnType::Uuid) => 2950, // uuid
            Some(ColumnType::Bytea) => 17,  // bytea
            Some(ColumnType::Enum(_)) => 3500, // enum
            None => 20,                     // default: int8
        }
    }
}

/// Collect column-level `REFERENCES` clauses from a slice of `ColumnDef`s
/// and convert each into a `TableForeignKey` entry (Task 3.4).
///
/// A column declared as `parent_id INT REFERENCES parent(id) ON DELETE CASCADE`
/// produces a `TableForeignKey { columns: ["parent_id"], ref_table: "parent",
/// ref_columns: ["id"], on_delete: Some(Cascade), on_update: None }`.
///
/// Table-level `FOREIGN KEY (...) REFERENCES ...` clauses are NOT handled
/// here — they are added by `from_create_table` directly from
/// `CreateTable.foreign_keys`.
fn column_fks_from_ddl(cols: &[crate::sql::ddl::ColumnDef]) -> Vec<crate::sql::ddl::TableForeignKey> {
    let mut fks = Vec::new();
    for c in cols {
        if let Some((ref_table, ref_col)) = &c.references {
            fks.push(crate::sql::ddl::TableForeignKey {
                columns: vec![c.name.clone()],
                ref_table: ref_table.clone(),
                ref_columns: vec![ref_col.clone()],
                on_delete: c.on_delete,
                on_update: c.on_update,
            });
        }
    }
    fks
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sql::ddl::ColumnType;

    #[test]
    fn from_ddl_preserves_types() {
        let cols = vec![
            crate::sql::ddl::ColumnDef {
                name: "id".into(),
                col_type: ColumnType::Int,
                not_null: true,
                primary_key: true,
                default: None,
                identity: false,
                references: None,
                unique: false,
                check: None,
                on_delete: None,
                on_update: None,
            },
            crate::sql::ddl::ColumnDef {
                name: "price".into(),
                col_type: ColumnType::Float,
                not_null: false,
                primary_key: false,
                default: None,
                identity: false,
                references: None,
                unique: false,
                check: None,
                on_delete: None,
                on_update: None,
            },
        ];
        let schema = TableSchema::from_ddl(&cols);
        assert_eq!(schema.columns.len(), 2);
        assert_eq!(schema.columns[0].name, "id");
        assert!(schema.columns[0].not_null);
        assert!(schema.columns[0].primary_key);
        assert_eq!(schema.col_type("price"), Some(&ColumnType::Float));
    }

    #[test]
    fn is_string_check() {
        let schema = TableSchema {
            columns: vec![
                ColumnSchema {
                    name: "id".into(),
                    col_type: ColumnType::Int,
                    not_null: false,
                    primary_key: false,
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
            ],
            checks: Vec::new(),
            unique_constraints: Vec::new(),
            foreign_keys: Vec::new(),
        };
        assert!(!schema.is_string(0));
        assert!(schema.is_string(1));
    }

    #[test]
    fn is_float_check() {
        let schema = TableSchema {
            columns: vec![
                ColumnSchema {
                    name: "id".into(),
                    col_type: ColumnType::Int,
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
            unique_constraints: Vec::new(),
            foreign_keys: Vec::new(),
        };
        assert!(!schema.is_float(0));
        assert!(schema.is_float(1));
    }

    #[test]
    fn format_float_cell() {
        let schema = TableSchema {
            columns: vec![ColumnSchema {
                name: "price".into(),
                col_type: ColumnType::Float,
                not_null: false,
                primary_key: false,
                unique: false,
                check: None,
            }],
            checks: Vec::new(),
            unique_constraints: Vec::new(),
            foreign_keys: Vec::new(),
        };
        let val = 19.99f64.to_bits();
        assert_eq!(schema.format_cell(0, val), "19.99");
    }

    #[test]
    fn format_int_cell() {
        let schema = TableSchema {
            columns: vec![ColumnSchema {
                name: "id".into(),
                col_type: ColumnType::Int,
                not_null: false,
                primary_key: false,
                unique: false,
                check: None,
            }],
            checks: Vec::new(),
            unique_constraints: Vec::new(),
            foreign_keys: Vec::new(),
        };
        assert_eq!(schema.format_cell(0, 42), "42");
    }

    #[test]
    fn format_bool_cell() {
        let schema = TableSchema {
            columns: vec![ColumnSchema {
                name: "active".into(),
                col_type: ColumnType::Boolean,
                not_null: false,
                primary_key: false,
                unique: false,
                check: None,
            }],
            checks: Vec::new(),
            unique_constraints: Vec::new(),
            foreign_keys: Vec::new(),
        };
        assert_eq!(schema.format_cell(0, 1), "true");
        assert_eq!(schema.format_cell(0, 0), "false");
    }

    #[test]
    fn pg_type_oid() {
        let schema = TableSchema {
            columns: vec![
                ColumnSchema {
                    name: "a".into(),
                    col_type: ColumnType::Int,
                    not_null: false,
                    primary_key: false,
                    unique: false,
                    check: None,
                },
                ColumnSchema {
                    name: "b".into(),
                    col_type: ColumnType::BigInt,
                    not_null: false,
                    primary_key: false,
                    unique: false,
                    check: None,
                },
                ColumnSchema {
                    name: "c".into(),
                    col_type: ColumnType::Float,
                    not_null: false,
                    primary_key: false,
                    unique: false,
                    check: None,
                },
                ColumnSchema {
                    name: "d".into(),
                    col_type: ColumnType::Varchar(Some(50)),
                    not_null: false,
                    primary_key: false,
                    unique: false,
                    check: None,
                },
                ColumnSchema {
                    name: "e".into(),
                    col_type: ColumnType::Boolean,
                    not_null: false,
                    primary_key: false,
                    unique: false,
                    check: None,
                },
            ],
            checks: Vec::new(),
            unique_constraints: Vec::new(),
            foreign_keys: Vec::new(),
        };
        assert_eq!(schema.pg_type_oid(0), 23); // int4
        assert_eq!(schema.pg_type_oid(1), 20); // int8
        assert_eq!(schema.pg_type_oid(2), 701); // float8
        assert_eq!(schema.pg_type_oid(3), 25); // text
        assert_eq!(schema.pg_type_oid(4), 16); // bool
    }
}
