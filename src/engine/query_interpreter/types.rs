//! Types for the query interpreter.
//!
//! Defines the legacy expression AST (Expr2, BinOp2, Value2) and query
//! types (SelectQuery2, FromItem, JoinClause2, etc.). Wave 4 will migrate
//! these to the unified `ast::Expr`.

use crate::catalog::Catalog;
use crate::datasource::csv::{tpc_h_schema, TpcHType};
use crate::datasource::table::Table;
use crate::engine::result::{QueryResult, ResultColumn};
use crate::exec::fm_index::StringSearchColumn;
use crate::sql::lexer::{tokenize, Token};
use crate::Error;
use fxhash::{FxHashMap, FxHashSet};
use rayon::prelude::*;

use super::{HashMap, HashSet, new_hashmap, new_hashset, new_fxhashmap, new_fxhashset};
use std::cell::{Cell, RefCell};

pub(crate) struct QueryInterpreter<'a> {
    pub(crate) catalog: &'a Catalog,
    /// Outer context for correlated subqueries: (outer_table_ptr, outer_row).
    /// Set when entering a subquery eval, restored after. Uses raw pointer
    /// for lifetime erasure (safe because the outer table is valid for the
    /// duration of the synchronous subquery execution).
    pub(crate) outer: std::cell::Cell<Option<(*const ExecTable, usize)>>,
    /// Cache for uncorrelated scalar subqueries: keyed by the SelectQuery2
    /// AST pointer (stable for the query's lifetime). Populated lazily by
    /// `precache_subqueries` (called at the start of `execute`) which tries
    /// to execute each subquery with outer=None — if it succeeds, the
    /// subquery is uncorrelated and the result is cached; if it fails
    /// (column not found), it's correlated and per-row eval handles it.
    /// This fixes Q11 (HAVING with uncorrelated scalar subquery) which
    /// previously re-executed the subquery per group (~8000x) and timed out.
    pub(crate) subquery_cache: std::cell::RefCell<HashMap<usize, Value2>>,
    /// Cache for EXISTS semi-join hash sets: keyed by the subquery AST pointer.
    /// When an EXISTS subquery has a single correlation column with an equi-join
    /// (e.g. Q4's `exists (SELECT * FROM lineitem WHERE l_orderkey = o_orderkey
    /// AND l_commitdate < l_receiptdate)`), we build a hash set of the inner
    /// column values (l_orderkey where l_commitdate < l_receiptdate) ONCE,
    /// then check membership per outer row. This decorrelates the EXISTS,
    /// reducing ~25k subquery executions to 1 hash-set build + 25k lookups.
    pub(crate) exists_cache: std::cell::RefCell<HashMap<usize, FxHashSet<u64>>>,
    /// Cache for multi-column EXISTS: HashMap<equi_key, HashSet<ineq_col>>.
    /// For Q21's `exists (SELECT * FROM lineitem l2 WHERE l2.l_orderkey = l1.l_orderkey
    /// AND l2.l_suppkey <> l1.l_suppkey)`, we build a HashMap<l_orderkey, HashSet<l_suppkey>>
    /// once, then for each outer row, check if any suppkey in the set != l1.l_suppkey.
    pub(crate) exists_multi_cache: std::cell::RefCell<HashMap<usize, FxHashMap<u64, FxHashSet<u64>>>>,
    /// Cache for uncorrelated IN-subquery result sets: keyed by AST pointer.
    /// When an IN-subquery is uncorrelated (e.g. Q20's `s_suppkey IN (SELECT
    /// ps_suppkey FROM partsupp WHERE ...)`), we execute it ONCE and cache
    /// the set of values. Then per-row eval just checks membership.
    pub(crate) in_subquery_cache: std::cell::RefCell<HashMap<usize, FxHashSet<u64>>>,
    /// Cache for decorrelated correlated scalar subqueries.
    /// When a correlated scalar subquery has an aggregate (sum/avg/min/max)
    /// and multiple correlation columns, we proactively build a derived table:
    /// execute the subquery's FROM table with local filters, GROUP BY the
    /// correlation columns, and cache a HashMap<corr_key_hash, agg_value>.
    /// Then per-row eval is a single hash lookup (O(1)) instead of a full
    /// subquery execution. This is critical for Q20 where the correlation
    /// key (ps_partkey, ps_suppkey) has 800k distinct values, each requiring
    /// a 6M-row lineitem scan — the derived table scans lineitem ONCE.
    /// Value: (HashMap<corr_hash, agg_value>, Vec<usize> corr_col_indices_in_outer).
    pub(crate) decorrelated_cache: std::cell::RefCell<HashMap<usize, (FxHashMap<u64, Value2>, Vec<usize>)>>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ColType {
    Int,
    Float,
    Date,
    String,
}

/// Swap comparison operands (for Literal op Col → Col swap_op(op) Literal).
pub(crate) fn swap_op(op: BinOp2) -> BinOp2 {
    match op {
        BinOp2::Lt => BinOp2::Gt,
        BinOp2::Le => BinOp2::Ge,
        BinOp2::Gt => BinOp2::Lt,
        BinOp2::Ge => BinOp2::Le,
        BinOp2::Eq => BinOp2::Eq,
        BinOp2::Ne => BinOp2::Ne,
        other => other,
    }
}

pub fn tpc_h_col_types(table_name: &str) -> Vec<ColType> {
    tpc_h_schema(table_name)
        .unwrap_or_default()
        .iter()
        .map(|(_, t)| match t {
            TpcHType::Int64 => ColType::Int,
            TpcHType::Float64 => ColType::Float,
            TpcHType::Date => ColType::Date,
            TpcHType::String => ColType::String,
        })
        .collect()
}

// =========================================================================
// AST
// =========================================================================

#[derive(Debug, Clone)]
pub enum Expr2 {
    Col(String),
    Int(i64),
    Float(f64),
    Str(String),
    Date(i32),
    BinOp {
        op: BinOp2,
        left: Box<Expr2>,
        right: Box<Expr2>,
    },
    Like {
        expr: Box<Expr2>,
        pattern: Box<Expr2>,
        negated: bool,
    },
    Between {
        expr: Box<Expr2>,
        low: Box<Expr2>,
        high: Box<Expr2>,
        negated: bool,
    },
    InList {
        expr: Box<Expr2>,
        list: Vec<Expr2>,
        negated: bool,
    },
    InSubquery {
        expr: Box<Expr2>,
        query: Box<SelectQuery2>,
        negated: bool,
    },
    Exists {
        query: Box<SelectQuery2>,
        negated: bool,
    },
    Case {
        whens: Vec<(Expr2, Expr2)>,
        else_: Option<Box<Expr2>>,
    },
    Extract {
        field: String,
        expr: Box<Expr2>,
    },
    /// `CAST(expr AS target_type)` (Wave 67). The target_type is an
    /// uppercased string ("INT", "FLOAT", "VARCHAR", "BIGINT").
    Cast {
        expr: Box<Expr2>,
        target_type: String,
    },
    Substr {
        expr: Box<Expr2>,
        start: Box<Expr2>,
        len: Box<Expr2>,
    },
    Agg {
        func: AggFunc,
        arg: Box<Expr2>,
        distinct: bool,
    },
    CountStar,
    Subquery(Box<SelectQuery2>),
    Not(Box<Expr2>),
    Neg(Box<Expr2>),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BinOp2 {
    Add,
    Sub,
    Mul,
    Div,
    Eq,
    Ne,
    Lt,
    Gt,
    Le,
    Ge,
    And,
    Or,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AggFunc {
    Sum,
    Avg,
    Count,
    Min,
    Max,
    CountDistinct,
}

#[derive(Debug, Clone)]
pub struct TableRef {
    pub name: String,
    pub alias: Option<String>,
}

#[derive(Debug, Clone)]
pub enum FromItem {
    Table(TableRef),
    Derived(Box<SelectQuery2>, Option<String>),
}

#[derive(Debug, Clone)]
pub struct JoinClause2 {
    pub join_type: JoinType2,
    pub table: FromItem,
    pub on: Expr2,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum JoinType2 {
    Inner,
    Left,
}

#[derive(Debug, Clone)]
pub struct SelectItem2 {
    pub expr: Expr2,
    pub alias: Option<String>,
}

#[derive(Debug, Clone)]
pub struct SelectQuery2 {
    pub select: Vec<SelectItem2>,
    pub from: Vec<FromItem>,
    pub joins: Vec<JoinClause2>,
    pub where_clause: Option<Expr2>,
    pub group_by: Vec<Expr2>,
    pub having: Option<Expr2>,
    pub order_by: Vec<(Expr2, bool)>,
    pub limit: Option<usize>,
}

// =========================================================================
// Parser
// =========================================================================


pub(crate) struct ExecTable {
    pub(crate) columns: Vec<std::sync::Arc<Vec<u64>>>,
    pub(crate) column_names: Vec<String>,
    pub(crate) col_types: Vec<ColType>,
    pub(crate) string_columns: Vec<Option<std::sync::Arc<StringSearchColumn>>>,
    pub(crate) row_count: usize,
    pub(crate) col_map: HashMap<String, usize>,
}

impl ExecTable {
    pub(crate) fn from_catalog(table: &Table, alias: &str) -> Self {
        // Wave 57 fix: tpc_h_col_types() returns an empty Vec for user-created
        // tables (it only knows TPC-H schemas). When that happens, we fall
        // back to inferring types from the table's schema (set by CREATE TABLE)
        // — defaulting to ColType::Int for unknown columns. Previously, the
        // empty Vec caused `t.col_types[idx]` to panic with index-out-of-bounds
        // whenever a CASE WHEN / arithmetic / string evaluation ran against a
        // user-created table through the interpreter fallback path.
        let mut col_types = tpc_h_col_types(&table.name);
        if col_types.is_empty() {
            col_types = (0..table.column_names.len())
                .map(|i| {
                    // Infer from the schema if available.
                    if let Some(ref schema) = table.schema {
                        if schema.is_float(i) {
                            ColType::Float
                        } else if schema.is_string(i) {
                            ColType::String
                        } else {
                            // Check for Date/Timestamp types.
                            match schema.col_type_at(i) {
                                Some(crate::sql::ddl::ColumnType::Date) => ColType::Date,
                                Some(crate::sql::ddl::ColumnType::Timestamp) => ColType::Date,
                                _ => ColType::Int,
                            }
                        }
                    } else {
                        ColType::Int
                    }
                })
                .collect();
        }
        let mut col_map = new_hashmap();
        for (i, name) in table.column_names.iter().enumerate() {
            let lower = name.to_lowercase();
            col_map.entry(name.to_lowercase()).or_insert(i);
            col_map.entry(format!("{}.{}", alias.to_lowercase(), name.to_lowercase())).or_insert(i);
            if alias != table.name {
                col_map
                    .entry(format!("{}.{}", table.name.to_lowercase(), name.to_lowercase()))
                    .or_insert(i);
            }
            let _ = lower; // suppress unused-variable warning
        }
        ExecTable {
            columns: table.columns.clone(),
            column_names: table.column_names.clone(),
            col_types,
            string_columns: table.string_columns.clone(),
            row_count: table.row_count,
            col_map,
        }
    }

    pub(crate) fn lookup_col(&self, name: &str) -> Option<usize> {
        // Fast path: direct lookup (common case — name is already lowercase
        // because col_map keys are stored lowercase).
        if let Some(&idx) = self.col_map.get(name) {
            return Some(idx);
        }
        // Slow path: case-insensitive lookup via to_lowercase.
        // Only reached for uppercase/mixed-case column names (rare in TPC-H).
        self.col_map.get(&name.to_lowercase()).copied()
    }
}


#[derive(Debug, Clone)]
pub(crate) enum Value2 {
    Int(i64),
    Float(f64),
    Str(String),
    Date(i32),
    Null,
}

impl Value2 {
    pub(crate) fn as_f64(&self) -> Option<f64> {
        match self {
            Value2::Int(i) => Some(*i as f64),
            Value2::Float(f) => Some(*f),
            Value2::Date(d) => Some(*d as f64),
            Value2::Null => None,
            Value2::Str(s) => s.parse().ok(),
        }
    }
    pub(crate) fn as_i64(&self) -> Option<i64> {
        match self {
            Value2::Int(i) => Some(*i),
            Value2::Float(f) => Some(*f as i64),
            Value2::Date(d) => Some(*d as i64),
            Value2::Null => None,
            Value2::Str(s) => s.parse().ok(),
        }
    }
    pub(crate) fn as_str(&self) -> Option<&str> {
        match self {
            Value2::Str(s) => Some(s),
            _ => None,
        }
    }
    pub(crate) fn as_u64(&self) -> Option<u64> {
        match self {
            Value2::Int(i) => Some(*i as u64),
            Value2::Float(f) => Some(*f as u64),
            Value2::Date(d) => Some(*d as u64),
            _ => None,
        }
    }
    pub(crate) fn to_u64(&self) -> u64 {
        match self {
            Value2::Int(i) => *i as u64,
            Value2::Float(f) => f.to_bits(),
            Value2::Date(d) => *d as u32 as u64,
            Value2::Null => 0,
            Value2::Str(s) => xxhash_rust::xxh3::xxh3_64(s.as_bytes()),
        }
    }
}


