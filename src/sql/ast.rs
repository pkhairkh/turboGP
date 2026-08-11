//! Unified Abstract Syntax Tree (AST) for turboGP.
//!
//! **Wave 3 (IR Unification, Part 1):** This module defines the single
//! canonical expression AST for turboGP, merging the former `sql::parser::Expr`
//! (6 variants) and `engine::query_interpreter::Expr2` (16 variants) into one unified type.
//!
//! ## Design
//!
//! The unified `Expr` enum has ~16 variants covering every SQL expression
//! shape the engine supports:
//!
//! - **Literals:** `Literal(Value)` where `Value` is `Int | Float | Str | Date | Null`
//! - **References:** `Column(String)`
//! - **Operators:** `Binary { op: BinOp, left, right }`, `Unary { op, expr }`, `Not(Expr)`
//! - **Predicates:** `Like`, `Between`, `InList`, `InSubquery`, `Exists`
//! - **Control flow:** `Case { when_clauses, else_clause }`
//! - **Functions:** `Function { name, args, distinct }` (covers aggregates and scalars)
//! - **SQL helpers:** `Extract { field, expr }`, `Cast { expr, target_type }`, `Wildcard`
//!
//! ## Migration path
//!
//! 1. **Wave 3 (this module):** Define the unified types. Existing `Expr` and
//!    `Expr2` remain in their modules for now.
//! 2. **Wave 3 (Task 3.3):** Refactor `sql::parser` to produce `ast::Expr`.
//!    Delete the old 6-variant `Expr`.
//! 3. **Wave 3 (Task 3.3):** Refactor `engine::query_interpreter` to consume `ast::Expr`.
//!    Delete `Expr2`, `Value2`, `BinOp2`.
//! 4. **Wave 4:** Expand `PlanNode` to carry `ast::Expr` in Filter/Project nodes.
//! 5. **Wave 5:** The lowerer translates `ast::Expr` in filter predicates into
//!    kernel-compatible `KernelParams`.
//!
//! ## References
//!
//! - Apache Calcite `RexNode` (the industry-standard relational expression IR)
//! - DuckDB `ParsedExpression` (30+ variants)
//! - Cascades framework (Graefe 1995) — expression equivalence classes

use std::fmt;

/// A unified SQL value literal.
///
/// Replaces the former `Value` (in `sql::parser`) and `Value2` (in
/// `engine::query_interpreter`). Every value is one of: 64-bit integer, 64-bit float,
/// UTF-8 string, days-since-epoch date, or NULL.
#[derive(Debug, Clone, PartialEq)]
pub enum Value {
    /// A 64-bit signed integer literal (e.g., `42`, `-1`).
    Int(i64),
    /// A 64-bit floating-point literal (e.g., `3.14`, `1e10`).
    Float(f64),
    /// A string literal (e.g., `'hello'`).
    Str(String),
    /// A date literal stored as days since the Unix epoch (e.g., `DATE '2024-01-15'`).
    Date(i32),
    /// SQL NULL.
    Null,
}

impl Value {
    /// Returns `true` if this value is NULL.
    pub fn is_null(&self) -> bool {
        matches!(self, Value::Null)
    }

    /// Convert this value to a `u64` cell (the engine's universal storage format).
    /// Int → as-is, Float → `to_bits`, Date → as u64, Str → 0 (strings are
    /// stored in a sidecar), Null → 0.
    pub fn to_cell(&self) -> u64 {
        match self {
            Value::Int(n) => *n as u64,
            Value::Float(f) => f.to_bits(),
            Value::Date(d) => *d as u64,
            Value::Str(_) => 0, // String hash stored in sidecar
            Value::Null => 0,
        }
    }
}

impl fmt::Display for Value {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Value::Int(n) => write!(f, "{n}"),
            Value::Float(fl) => write!(f, "{fl}"),
            Value::Str(s) => write!(f, "'{}'", s.replace('\'', "''")),
            Value::Date(d) => write!(f, "DATE '{d}'"),
            Value::Null => write!(f, "NULL"),
        }
    }
}

/// A binary operator.
///
/// Replaces the string-based `op: String` in the old `Expr::Binary` and
/// the `BinOp2` enum in `Expr2`. Using a typed enum prevents typos and
/// enables exhaustive pattern matching.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum BinOp {
    // Comparison
    Eq,
    NotEq,
    Lt,
    Gt,
    LtEq,
    GtEq,
    // Logical
    And,
    Or,
    // Arithmetic
    Add,
    Sub,
    Mul,
    Div,
    Mod,
    // String
    Concat,
}

impl BinOp {
    /// Parse a binary operator from a SQL token string.
    pub fn from_str(op: &str) -> Option<Self> {
        match op.to_uppercase().as_str() {
            "=" | "==" => Some(BinOp::Eq),
            "!=" | "<>" => Some(BinOp::NotEq),
            "<" => Some(BinOp::Lt),
            ">" => Some(BinOp::Gt),
            "<=" => Some(BinOp::LtEq),
            ">=" => Some(BinOp::GtEq),
            "AND" => Some(BinOp::And),
            "OR" => Some(BinOp::Or),
            "+" => Some(BinOp::Add),
            "-" => Some(BinOp::Sub),
            "*" => Some(BinOp::Mul),
            "/" => Some(BinOp::Div),
            "%" => Some(BinOp::Mod),
            "||" => Some(BinOp::Concat),
            _ => None,
        }
    }

    /// Returns the SQL string representation of this operator.
    pub fn as_str(&self) -> &'static str {
        match self {
            BinOp::Eq => "=",
            BinOp::NotEq => "!=",
            BinOp::Lt => "<",
            BinOp::Gt => ">",
            BinOp::LtEq => "<=",
            BinOp::GtEq => ">=",
            BinOp::And => "AND",
            BinOp::Or => "OR",
            BinOp::Add => "+",
            BinOp::Sub => "-",
            BinOp::Mul => "*",
            BinOp::Div => "/",
            BinOp::Mod => "%",
            BinOp::Concat => "||",
        }
    }

    /// Returns `true` if this is a comparison operator (=, !=, <, >, <=, >=).
    pub fn is_comparison(&self) -> bool {
        matches!(self, BinOp::Eq | BinOp::NotEq | BinOp::Lt | BinOp::Gt | BinOp::LtEq | BinOp::GtEq)
    }

    /// Returns `true` if this is a logical operator (AND, OR).
    pub fn is_logical(&self) -> bool {
        matches!(self, BinOp::And | BinOp::Or)
    }

    /// Returns `true` if this is an arithmetic operator (+, -, *, /, %).
    pub fn is_arithmetic(&self) -> bool {
        matches!(self, BinOp::Add | BinOp::Sub | BinOp::Mul | BinOp::Div | BinOp::Mod)
    }
}

impl fmt::Display for BinOp {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.as_str())
    }
}

/// A unary operator.
///
/// Used by `Expr::Unary`. Currently only `Neg` (prefix `-`) is supported;
/// `Pos` (prefix `+`) is included for completeness and may be used by
/// future parsers that distinguish `+x` from `x`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum UnaryOp {
    /// Unary negation: `-expr`.
    Neg,
    /// Unary plus: `+expr` (no-op, preserved for AST fidelity).
    Pos,
}

impl UnaryOp {
    /// Returns the SQL string representation of this operator.
    pub fn as_str(&self) -> &'static str {
        match self {
            UnaryOp::Neg => "-",
            UnaryOp::Pos => "+",
        }
    }
}

impl fmt::Display for UnaryOp {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.as_str())
    }
}

/// The unified SQL expression AST.
///
/// This single type replaces the former `sql::parser::Expr` (6 variants)
/// and `engine::query_interpreter::Expr2` (16 variants). Every SQL expression — from
/// `SELECT 1` to `CASE WHEN EXISTS (SELECT ...) THEN ... END` — is
/// represented as an `Expr`.
///
/// # Design principles
///
/// - **No string-typed operators.** Use `BinOp` enum for type safety.
/// - **No duplicate literal types.** Use `Value` for all literals.
/// - **Negated predicates are explicit.** `Like { negated: true }` is
///   `NOT LIKE`, not a separate `NotLike` variant.
/// - **Subqueries are first-class.** `InSubquery` and `Exists` carry a
///   `Box<SelectQuery>` (forward-declared via `SelectQueryRef` to avoid
///   a circular dependency with `sql::parser`).
#[derive(Debug, Clone, PartialEq)]
pub enum Expr {
    /// A column reference (e.g., `name`, `t.price`).
    Column(String),

    /// A literal value (e.g., `42`, `3.14`, `'hello'`, `NULL`).
    Literal(Value),

    /// A binary operator application: `left op right`.
    Binary {
        /// Left operand.
        left: Box<Expr>,
        /// The operator (typed, not string).
        op: BinOp,
        /// Right operand.
        right: Box<Expr>,
    },

    /// A unary operator application: `op expr` (e.g., `-x`, `+x`).
    Unary {
        /// The operator (typed).
        op: UnaryOp,
        /// The operand.
        expr: Box<Expr>,
    },

    /// Logical NOT: `NOT expr`.
    Not(Box<Expr>),

    /// A CASE WHEN expression.
    /// Evaluates WHEN clauses in order; returns the first matching THEN
    /// value, or the ELSE value if no WHEN matches.
    Case {
        /// List of (condition, result) pairs.
        when_clauses: Vec<(Expr, Expr)>,
        /// Optional ELSE clause (defaults to NULL).
        else_clause: Option<Box<Expr>>,
    },

    /// A function or aggregate call (e.g., `COUNT(*)`, `SUM(price)`,
    /// `UPPER(name)`).
    Function {
        /// The function name, uppercased (e.g., `COUNT`, `SUM`, `UPPER`).
        name: String,
        /// Arguments (empty for `COUNT(*)` which is represented as
        /// `Function { name: "COUNT", args: vec![Expr::Wildcard], distinct: false }`).
        args: Vec<Expr>,
        /// `true` for `COUNT(DISTINCT col)`, `SUM(DISTINCT col)`, etc.
        distinct: bool,
    },

    /// `expr LIKE pattern` (or `NOT LIKE` if `negated`).
    Like {
        /// The expression to test.
        expr: Box<Expr>,
        /// The pattern (e.g., `'%foo%'`).
        pattern: Box<Expr>,
        /// `true` for `NOT LIKE`.
        negated: bool,
    },

    /// `expr BETWEEN low AND high` (or `NOT BETWEEN` if `negated`).
    Between {
        /// The expression to test.
        expr: Box<Expr>,
        /// Lower bound (inclusive).
        low: Box<Expr>,
        /// Upper bound (inclusive).
        high: Box<Expr>,
        /// `true` for `NOT BETWEEN`.
        negated: bool,
    },

    /// `expr IN (val1, val2, ...)` (or `NOT IN` if `negated`).
    InList {
        /// The expression to test.
        expr: Box<Expr>,
        /// The list of values.
        list: Vec<Expr>,
        /// `true` for `NOT IN`.
        negated: bool,
    },

    /// `expr IN (SELECT ...)` (or `NOT IN` if `negated`).
    /// Uses a string representation of the subquery SQL to avoid a
    /// circular dependency with `sql::parser::SelectQuery`.
    InSubquery {
        /// The expression to test.
        expr: Box<Expr>,
        /// The subquery SQL string (re-parsed by the executor when needed).
        subquery_sql: String,
        /// `true` for `NOT IN (SELECT ...)`.
        negated: bool,
    },

    /// `EXISTS (SELECT ...)` (or `NOT EXISTS` if `negated`).
    Exists {
        /// The subquery SQL string.
        subquery_sql: String,
        /// `true` for `NOT EXISTS`.
        negated: bool,
    },

    /// `EXTRACT(field FROM expr)` — extract a sub-field from a date/timestamp.
    Extract {
        /// The field to extract (e.g., `YEAR`, `MONTH`, `DAY`).
        field: String,
        /// The source expression.
        expr: Box<Expr>,
    },

    /// `CAST(expr AS target_type)` — type conversion.
    Cast {
        /// The expression to cast.
        expr: Box<Expr>,
        /// The target SQL type name (e.g., `INT`, `FLOAT`, `VARCHAR`).
        target_type: String,
    },

    /// `*` — used in `COUNT(*)` and `SELECT *`.
    Wildcard,

    /// A parenthesised sub-expression. This is transparent — the inner
    /// expr is the semantic value — but preserved for pretty-printing.
    Paren(Box<Expr>),
}

impl Expr {
    /// Convenience constructor for a column reference.
    pub fn col(name: impl Into<String>) -> Self {
        Expr::Column(name.into())
    }

    /// Convenience constructor for an integer literal.
    pub fn int(n: i64) -> Self {
        Expr::Literal(Value::Int(n))
    }

    /// Convenience constructor for a float literal.
    pub fn float(f: f64) -> Self {
        Expr::Literal(Value::Float(f))
    }

    /// Convenience constructor for a string literal.
    pub fn str(s: impl Into<String>) -> Self {
        Expr::Literal(Value::Str(s.into()))
    }

    /// Convenience constructor for NULL.
    pub fn null() -> Self {
        Expr::Literal(Value::Null)
    }

    /// Convenience constructor for a binary expression.
    pub fn binary(left: Expr, op: BinOp, right: Expr) -> Self {
        Expr::Binary { left: Box::new(left), op, right: Box::new(right) }
    }

    /// Returns `true` if this expression is a literal (including NULL).
    pub fn is_literal(&self) -> bool {
        matches!(self, Expr::Literal(_))
    }

    /// Returns `true` if this expression is an aggregate function call
    /// (COUNT, SUM, AVG, MIN, MAX).
    pub fn is_aggregate(&self) -> bool {
        match self {
            Expr::Function { name, .. } => {
                matches!(name.to_uppercase().as_str(), "COUNT" | "SUM" | "AVG" | "MIN" | "MAX")
            }
            _ => false,
        }
    }

    /// Collect all column references in this expression (recursively).
    /// Useful for projection pruning and predicate analysis.
    pub fn columns(&self) -> Vec<String> {
        match self {
            Expr::Column(name) => vec![name.clone()],
            Expr::Literal(_) | Expr::Wildcard => vec![],
            Expr::Binary { left, right, .. } => {
                let mut cols = left.columns();
                cols.extend(right.columns());
                cols
            }
            Expr::Unary { expr: e, .. } | Expr::Not(e) | Expr::Paren(e) => e.columns(),
            Expr::Case { when_clauses, else_clause } => {
                let mut cols = Vec::new();
                for (cond, val) in when_clauses {
                    cols.extend(cond.columns());
                    cols.extend(val.columns());
                }
                if let Some(e) = else_clause {
                    cols.extend(e.columns());
                }
                cols
            }
            Expr::Function { args, .. } => args.iter().flat_map(|a| a.columns()).collect(),
            Expr::Like { expr, pattern, .. } => {
                let mut cols = expr.columns();
                cols.extend(pattern.columns());
                cols
            }
            Expr::Between { expr, low, high, .. } => {
                let mut cols = expr.columns();
                cols.extend(low.columns());
                cols.extend(high.columns());
                cols
            }
            Expr::InList { expr, list, .. } => {
                let mut cols = expr.columns();
                for v in list {
                    cols.extend(v.columns());
                }
                cols
            }
            Expr::InSubquery { expr, .. } => expr.columns(),
            Expr::Exists { .. } => vec![],
            Expr::Extract { expr, .. } => expr.columns(),
            Expr::Cast { expr, .. } => expr.columns(),
        }
    }
}

impl fmt::Display for Expr {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Expr::Column(name) => write!(f, "{name}"),
            Expr::Literal(v) => write!(f, "{v}"),
            Expr::Binary { left, op, right } => write!(f, "({left} {op} {right})"),
            Expr::Unary { op, expr } => write!(f, "({op}{expr})"),
            Expr::Not(e) => write!(f, "NOT {e}"),
            Expr::Case { when_clauses, else_clause } => {
                write!(f, "CASE")?;
                for (cond, val) in when_clauses {
                    write!(f, " WHEN {cond} THEN {val}")?;
                }
                if let Some(e) = else_clause {
                    write!(f, " ELSE {e}")?;
                }
                write!(f, " END")
            }
            Expr::Function { name, args, distinct } => {
                write!(f, "{name}(")?;
                if *distinct {
                    write!(f, "DISTINCT ")?;
                }
                if args.is_empty() || args.iter().all(|a| *a == Expr::Wildcard) {
                    write!(f, "*")?;
                } else {
                    let strs: Vec<String> = args.iter().map(|a| a.to_string()).collect();
                    write!(f, "{}", strs.join(", "))?;
                }
                write!(f, ")")
            }
            Expr::Like { expr, pattern, negated } => {
                write!(f, "{expr}")?;
                if *negated {
                    write!(f, " NOT")?;
                }
                write!(f, " LIKE {pattern}")
            }
            Expr::Between { expr, low, high, negated } => {
                write!(f, "{expr}")?;
                if *negated {
                    write!(f, " NOT")?;
                }
                write!(f, " BETWEEN {low} AND {high}")
            }
            Expr::InList { expr, list, negated } => {
                write!(f, "{expr}")?;
                if *negated {
                    write!(f, " NOT")?;
                }
                let strs: Vec<String> = list.iter().map(|v| v.to_string()).collect();
                write!(f, " IN ({})", strs.join(", "))
            }
            Expr::InSubquery { expr, negated, .. } => {
                write!(f, "{expr}")?;
                if *negated {
                    write!(f, " NOT")?;
                }
                write!(f, " IN (SELECT ...)")
            }
            Expr::Exists { negated, .. } => {
                if *negated {
                    write!(f, "NOT ")?;
                }
                write!(f, "EXISTS (SELECT ...)")
            }
            Expr::Extract { field, expr } => write!(f, "EXTRACT({field} FROM {expr})"),
            Expr::Cast { expr, target_type } => write!(f, "CAST({expr} AS {target_type})"),
            Expr::Wildcard => write!(f, "*"),
            Expr::Paren(e) => write!(f, "({e})"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_value_to_cell_int() {
        assert_eq!(Value::Int(42).to_cell(), 42);
        assert_eq!(Value::Int(-1).to_cell(), u64::MAX);
    }

    #[test]
    fn test_value_to_cell_float() {
        let f = 3.14_f64;
        assert_eq!(Value::Float(f).to_cell(), f.to_bits());
    }

    #[test]
    fn test_value_to_cell_null() {
        assert_eq!(Value::Null.to_cell(), 0);
    }

    #[test]
    fn test_value_display() {
        assert_eq!(Value::Int(42).to_string(), "42");
        assert_eq!(Value::Str("hello".into()).to_string(), "'hello'");
        assert_eq!(Value::Str("it's".into()).to_string(), "'it''s'");
        assert_eq!(Value::Null.to_string(), "NULL");
    }

    #[test]
    fn test_binop_from_str() {
        assert_eq!(BinOp::from_str("="), Some(BinOp::Eq));
        assert_eq!(BinOp::from_str("!="), Some(BinOp::NotEq));
        assert_eq!(BinOp::from_str("<>"), Some(BinOp::NotEq));
        assert_eq!(BinOp::from_str("AND"), Some(BinOp::And));
        assert_eq!(BinOp::from_str("and"), Some(BinOp::And));
        assert_eq!(BinOp::from_str("||"), Some(BinOp::Concat));
        assert_eq!(BinOp::from_str("unknown"), None);
    }

    #[test]
    fn test_binop_classification() {
        assert!(BinOp::Eq.is_comparison());
        assert!(!BinOp::Eq.is_arithmetic());
        assert!(BinOp::And.is_logical());
        assert!(BinOp::Add.is_arithmetic());
        assert!(!BinOp::Add.is_logical());
    }

    #[test]
    fn test_expr_constructors() {
        assert_eq!(Expr::col("name"), Expr::Column("name".into()));
        assert_eq!(Expr::int(42), Expr::Literal(Value::Int(42)));
        assert_eq!(Expr::float(3.14), Expr::Literal(Value::Float(3.14)));
        assert_eq!(Expr::str("hi"), Expr::Literal(Value::Str("hi".into())));
        assert_eq!(Expr::null(), Expr::Literal(Value::Null));
    }

    #[test]
    fn test_expr_is_aggregate() {
        assert!(Expr::Function {
            name: "COUNT".into(),
            args: vec![Expr::Wildcard],
            distinct: false,
        }
        .is_aggregate());
        assert!(Expr::Function {
            name: "sum".into(),
            args: vec![Expr::col("price")],
            distinct: false,
        }
        .is_aggregate());
        assert!(!Expr::Function {
            name: "UPPER".into(),
            args: vec![Expr::col("name")],
            distinct: false,
        }
        .is_aggregate());
        assert!(!Expr::col("x").is_aggregate());
    }

    #[test]
    fn test_expr_columns() {
        let e = Expr::binary(
            Expr::col("a"),
            BinOp::Eq,
            Expr::binary(Expr::col("b"), BinOp::Add, Expr::int(1)),
        );
        let cols = e.columns();
        assert!(cols.contains(&"a".to_string()));
        assert!(cols.contains(&"b".to_string()));
        assert_eq!(cols.len(), 2);
    }

    #[test]
    fn test_expr_display_binary() {
        let e = Expr::binary(Expr::col("x"), BinOp::Eq, Expr::int(42));
        assert_eq!(e.to_string(), "(x = 42)");
    }

    #[test]
    fn test_expr_display_function() {
        let e =
            Expr::Function { name: "COUNT".into(), args: vec![Expr::Wildcard], distinct: false };
        assert_eq!(e.to_string(), "COUNT(*)");
    }

    #[test]
    fn test_expr_display_like() {
        let e = Expr::Like {
            expr: Box::new(Expr::col("name")),
            pattern: Box::new(Expr::str("%foo%")),
            negated: false,
        };
        assert_eq!(e.to_string(), "name LIKE '%foo%'");

        let e = Expr::Like {
            expr: Box::new(Expr::col("name")),
            pattern: Box::new(Expr::str("%foo%")),
            negated: true,
        };
        assert_eq!(e.to_string(), "name NOT LIKE '%foo%'");
    }

    #[test]
    fn test_expr_display_unary() {
        let e = Expr::Unary {
            op: UnaryOp::Neg,
            expr: Box::new(Expr::int(1)),
        };
        assert_eq!(e.to_string(), "(-1)");
    }

    #[test]
    fn test_expr_unary_columns() {
        let e = Expr::Unary {
            op: UnaryOp::Neg,
            expr: Box::new(Expr::col("x")),
        };
        let cols = e.columns();
        assert_eq!(cols, vec!["x".to_string()]);
    }

    #[test]
    fn test_unary_op_display() {
        assert_eq!(UnaryOp::Neg.to_string(), "-");
        assert_eq!(UnaryOp::Pos.to_string(), "+");
    }

    #[test]
    fn test_expr_display_case() {
        let e = Expr::Case {
            when_clauses: vec![(Expr::col("x"), Expr::int(1))],
            else_clause: Some(Box::new(Expr::int(0))),
        };
        assert_eq!(e.to_string(), "CASE WHEN x THEN 1 ELSE 0 END");
    }
}
