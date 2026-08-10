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
}

/// Schema for a table: column schemas in order.
#[derive(Debug, Clone, Default)]
pub struct TableSchema {
    pub columns: Vec<ColumnSchema>,
}

impl TableSchema {
    pub fn new() -> Self {
        Self { columns: Vec::new() }
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
                })
                .collect(),
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
            },
            crate::sql::ddl::ColumnDef {
                name: "price".into(),
                col_type: ColumnType::Float,
                not_null: false,
                primary_key: false,
                default: None,
                identity: false,
                references: None,
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
                },
                ColumnSchema {
                    name: "name".into(),
                    col_type: ColumnType::Varchar(Some(50)),
                    not_null: false,
                    primary_key: false,
                },
            ],
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
                },
                ColumnSchema {
                    name: "price".into(),
                    col_type: ColumnType::Float,
                    not_null: false,
                    primary_key: false,
                },
            ],
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
            }],
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
            }],
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
            }],
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
                },
                ColumnSchema {
                    name: "b".into(),
                    col_type: ColumnType::BigInt,
                    not_null: false,
                    primary_key: false,
                },
                ColumnSchema {
                    name: "c".into(),
                    col_type: ColumnType::Float,
                    not_null: false,
                    primary_key: false,
                },
                ColumnSchema {
                    name: "d".into(),
                    col_type: ColumnType::Varchar(Some(50)),
                    not_null: false,
                    primary_key: false,
                },
                ColumnSchema {
                    name: "e".into(),
                    col_type: ColumnType::Boolean,
                    not_null: false,
                    primary_key: false,
                },
            ],
        };
        assert_eq!(schema.pg_type_oid(0), 23); // int4
        assert_eq!(schema.pg_type_oid(1), 20); // int8
        assert_eq!(schema.pg_type_oid(2), 701); // float8
        assert_eq!(schema.pg_type_oid(3), 25); // text
        assert_eq!(schema.pg_type_oid(4), 16); // bool
    }
}
