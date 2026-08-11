# Agent A — SQL Frontend API Change Requests

**Document for:** Integration Agent (merges `feat/sql-frontend`, `feat/storage-txn`, `feat/engine-planner` into `main`)
**Author:** Agent A — SQL Frontend
**Branch:** `feat/sql-frontend`
**Date:** Wave 8 completion

This document lists every API change made by the SQL Frontend remediation
(Waves 1–8) that affects code owned by Agent B (`src/storage/`, `src/txn/`)
or Agent C (`src/engine/`, `src/planner/`, `src/exec/`, `src/server/`,
`src/catalog/`, `src/kernel/`).

## 1. Unified AST Types

The 7-variant `parser::Expr` and 4-variant `parser::Value` enums have been
deleted. They are now re-exports of the unified `ast::Expr` (17 variants)
and `ast::Value` (6 variants).

### `crate::sql::ast::Expr` (re-exported as `crate::sql::Expr` and `crate::sql::parser::Expr`)

```rust
pub enum Expr {
    Column(String),
    Literal(Value),
    Binary { left: Box<Expr>, op: BinOp, right: Box<Expr> },
    Unary { op: UnaryOp, expr: Box<Expr> },          // NEW (Wave 2)
    Not(Box<Expr>),                                    // NEW (Wave 2)
    Case { when_clauses: Vec<(Expr, Expr)>, else_clause: Option<Box<Expr>> },
    Function { name: String, args: Vec<Expr>, distinct: bool },  // args changed from String
    Like { expr: Box<Expr>, pattern: Box<Expr>, negated: bool },  // NEW (Wave 2)
    Between { expr: Box<Expr>, low: Box<Expr>, high: Box<Expr>, negated: bool },  // NEW
    InList { expr: Box<Expr>, list: Vec<Expr>, negated: bool },   // NEW (Wave 2)
    IsNull { expr: Box<Expr>, negated: bool },                     // NEW (Wave 3)
    InSubquery { expr: Box<Expr>, subquery_sql: String, negated: bool },  // NEW (Wave 4)
    Exists { subquery_sql: String, negated: bool },                       // NEW (Wave 4)
    Extract { field: String, expr: Box<Expr> },
    Cast { expr: Box<Expr>, target_type: String },
    Wildcard,
    Paren(Box<Expr>),                                   // NEW (Wave 2)
}
```

### `crate::sql::ast::Value`

```rust
pub enum Value {
    Int(i64),
    Float(f64),
    String(String),   // renamed from Str
    Date(i32),        // NEW
    Hex(Vec<u8>),     // NEW (carried over from old parser::Value)
    Null,             // NEW
}
```

### `crate::sql::ast::BinOp` and `UnaryOp`

`BinOp` replaces the string-typed `op` field in `Expr::Binary`. Consumers
must update from `op == "=".to_string()` to `*op == BinOp::Eq` (or
`op.as_str() == "="`).

```rust
pub enum BinOp { Eq, NotEq, Lt, Gt, LtEq, GtEq, And, Or, Add, Sub, Mul, Div, Mod, Concat }
pub enum UnaryOp { Neg, Pos }
```

### `crate::sql::ast::SetQuery` (NEW — Wave 4)

```rust
pub enum SetQuery {
    Select(SelectQuery),
    Union(Box<SetQuery>, Box<SetQuery>),
    UnionAll(Box<SetQuery>, Box<SetQuery>),
    Intersect(Box<SetQuery>, Box<SetQuery>),
    Except(Box<SetQuery>, Box<SetQuery>),
}
```

Helpers: `as_select() -> Option<&SelectQuery>`, `into_select() -> Result<SelectQuery, String>`.

## 2. `SelectQuery` Changes

```rust
pub struct SelectQuery {
    pub select: Vec<SelectItem>,
    pub from: String,
    pub joins: Vec<JoinClause>,
    pub where_clause: Option<Expr>,         // was Option<Expr> (old parser::Expr)
    pub group_by: Vec<String>,
    pub having: Option<Expr>,
    pub order_by: Vec<(String, bool, NullsOrder)>,  // CHANGED: was Vec<(String, bool)>
    pub limit: Option<usize>,
    pub offset: Option<usize>,              // NEW (Wave 8)
    pub fetch: Option<usize>,               // NEW (Wave 8)
    pub distinct: bool,
    pub distinct_on: Option<Vec<String>>,   // NEW (Wave 8)
}

pub enum NullsOrder { First, Last, Default }  // NEW (Wave 8)
```

**Consumer action:** Update all `let (col, asc) = &order_by[i]` patterns
to `let (col, asc, _nulls) = &order_by[i]`. Update helper functions that
take `&[(String, bool)]` to `&[(String, bool, NullsOrder)]`.

## 3. New Parser Entry Points

| Function | Returns | Purpose |
|----------|---------|---------|
| `parse(tokens) -> Result<SelectQuery, String>` | `SelectQuery` | Backward-compatible; returns Err for set operations. |
| `parse_set(tokens) -> Result<SetQuery, String>` | `SetQuery` | NEW (Wave 4) — handles UNION/INTERSECT/EXCEPT. |
| `parse_expression(tokens) -> Result<Expr, String>` | `Expr` | NEW (Wave 5) — parses a single expression (used by DML). |

## 4. DML Type Changes

```rust
pub struct Update {
    pub table: String,
    pub assignments: Vec<(String, Expr)>,   // CHANGED: was Vec<(String, String)>
    pub where_clause: Option<Expr>,         // CHANGED: was Option<String>
    pub returning: Option<Vec<SelectItem>>, // NEW (Wave 5)
}

pub struct Delete {
    pub table: String,
    pub where_clause: Option<Expr>,         // CHANGED: was Option<String>
    pub returning: Option<Vec<SelectItem>>, // NEW (Wave 5)
}

pub struct Insert {
    pub table: String,
    pub columns: Option<Vec<String>>,
    pub values: Vec<Vec<String>>,
    pub source: InsertSource,               // NEW (Wave 5)
    pub returning: Option<Vec<SelectItem>>, // NEW (Wave 5)
    pub on_conflict: Option<OnConflict>,    // NEW (Wave 5)
}

pub enum InsertSource { Values, Select(SetQuery) }             // NEW (Wave 5)
pub enum OnConflict { DoNothing, DoUpdate { ... } }            // NEW (Wave 5)
```

**Consumer action (`engine/dml.rs`):** Convert `Expr` to SQL string via
`expr.to_string()` for the existing `parse_value_cell` / `eval_simple_where`
helpers. A proper refactor would update those helpers to accept `&Expr`
directly (deferred).

## 5. DDL Type Changes

```rust
pub struct ColumnDef {
    pub name: String,
    pub col_type: ColumnType,
    pub not_null: bool,
    pub primary_key: bool,
    pub default: Option<String>,
    pub identity: bool,
    pub references: Option<(String, String)>,
    pub unique: bool,                          // NEW (Wave 6)
    pub check: Option<crate::sql::ast::Expr>,  // NEW (Wave 6)
    pub on_delete: Option<ForeignKeyAction>,   // NEW (Wave 6)
    pub on_update: Option<ForeignKeyAction>,   // NEW (Wave 6)
}

pub enum ForeignKeyAction { Cascade, SetNull, SetDefault, Restrict, NoAction }  // NEW (Wave 6)
pub struct TableForeignKey { ... }  // NEW (Wave 6)

pub struct CreateTable {
    pub schema: String,
    pub name: String,
    pub columns: Vec<ColumnDef>,
    pub if_not_exists: bool,
    pub checks: Vec<crate::sql::ast::Expr>,         // NEW (Wave 6)
    pub unique_constraints: Vec<Vec<String>>,       // NEW (Wave 6)
    pub primary_key: Option<Vec<String>>,           // NEW (Wave 6)
    pub foreign_keys: Vec<TableForeignKey>,         // NEW (Wave 6)
}

pub struct CreateIndex {
    pub index_name: String,
    pub table: String,
    pub column: String,                            // backward compat (single-col)
    pub if_not_exists: bool,
    pub unique: bool,                              // NEW (Wave 6)
    pub columns: Vec<(String, bool)>,              // NEW (Wave 6) — multi-col + sort order
}
```

## 6. CTE Type Changes

```rust
pub struct CteDef {
    pub name: String,
    pub columns: Option<Vec<String>>,              // NEW (Wave 7)
    pub anchor: String,                            // now re-serialised from tokens
    pub recursive: Option<String>,                 // now re-serialised from tokens
    pub body: Option<SetQuery>,                    // NEW (Wave 7) — parsed body
}

pub struct WithClause {
    pub ctes: Vec<CteDef>,
    pub outer_query: String,                       // now re-serialised from tokens
    pub outer: Option<SetQuery>,                   // NEW (Wave 7) — parsed outer query
    pub max_recursion: u32,
}
```

## 7. Lexer Changes

New tokens:
- `Token::Param(u16)` — positional parameter `$1`, `$2`
- `Token::QuestionMark` — anonymous parameter `?`

New operators: `||`, `%`, `::`, `->`, `->>`

New keywords (added to `KEYWORDS`): `RETURNING`, `CONFLICT`, `NOTHING`,
`DO`, `UNIQUE`, `CHECK`, `CASCADE`, `ACTION`, `INTERSECT`, `EXCEPT`,
`NULLS`, `FIRST`, `LAST`, `NEXT`, `FETCH`, `OFFSET`, `ONLY`.

Comments (`--`, `/* */` with nesting), quoted identifiers (`"..."`),
escape strings (`E'...'`), and typed literals (`DATE '...'`) are now
supported by the lexer.

## 8. Files Modified Outside `src/sql/` (Build-Must-Pass Rule)

The following files outside `src/sql/` were modified to keep the build
green after the unified AST migration. These are mechanical changes
(replacing `op: String` with `op: BinOp`, adding missing match arms)
that preserve existing behavior.

- `src/engine/executor.rs` — `parse_expr` uses `BinOp`; `where_clause_to_expr`
  constructs `Expr::binary(...)` instead of `Expr::Binary { op: String }`.
- `src/engine/dispatch.rs` — `expr_needs_interpreter_fallback` covers all
  new `Expr` variants; `eval_predicate_mask` uses `BinOp` directly.
- `src/engine/helpers.rs` — `extract_eq_predicate` compares `BinOp::Eq`;
  `literal_to_cell` covers `Value::Date` and `Value::Null`.
- `src/engine/dml.rs` — converts `Expr` to SQL string for existing helpers.
- `src/exec/vectorized.rs` — `eval_where` uses `BinOp`; `value_to_u64`
  covers `Value::Date` and `Value::Null`.
- `src/exec/join.rs` — `collect_keys` uses `BinOp::And` / `BinOp::Eq`.
- `src/planner/plan_builder.rs` — `convert_expr` / `convert_value` collapse
  to trivial clones; `convert_op` delegates to `BinOp::from_str`.
- `src/schema/table_schema.rs` — test updated for new `ColumnDef` fields.

## 9. Deviations from Spec

The following deviations were made for backward compatibility (documented
here so the integration agent can decide whether to address them):

1. **`SelectItem::Aggregate { arg: String }` retained.** The spec (Task 2.3)
   called for `ast::Expr` args. The Aggregate variant is kept with `arg:
   String` for backward compat with the executor's aggregate shorthand.
   The unified `Expr::Function { name, args, distinct }` is available via
   `SelectItem::Expression` for general function calls.

2. **`parse()` returns `SelectQuery`, not `SetQuery`.** The spec (Task 4.5)
   called for `parse()` to return `SetQuery`. To avoid breaking 24 callers
   in `src/engine/`, `parse()` is preserved as a wrapper that delegates to
   `parse_set()` and extracts the single `SelectQuery` (returning `Err`
   for set operations). New code should use `parse_set()` directly.

3. **Scalar subqueries stored as `Expr::Function` with name
   `__scalar_subquery__`.** The spec (Task 4.1) suggested a dedicated
   `Expr::Subquery` variant or a re-parseable SQL string. To avoid adding
   a new `Expr` variant that all consumers would need to handle, scalar
   subqueries are stored as a synthetic `Function` with the subquery SQL
   as a `Value::String` argument. The executor can detect this convention.

4. **`engine/dml.rs` converts `Expr` to SQL string.** The spec (Task 5.1)
   implied the executor would evaluate `Expr` directly. The existing
   `parse_value_cell` and `eval_simple_where` helpers still accept `&str`,
   so `Expr::to_string()` is used as a bridge. A proper refactor would
   update those helpers to accept `&Expr` directly.

5. **Wave 3 Tasks 3.2, 3.3, 3.5 were substantially implemented in Wave 2.**
   The unified AST migration (Wave 2) added `Not`, `Paren`, `Unary`, and
   the multi-arg `Function` variants as part of the `ast::Expr` unification.
   Wave 3's parser changes (NOT prefix, unary minus, qualified columns,
   scalar functions) were applied on top in a single combined commit.

6. **Wave 3 Task 3.4: `t.*` stored as `Expr::Column("t.*")`.** The spec
   suggested `Expr::QualifiedWildcard(String)`. To avoid a new variant,
   the dotted name is stored as a `Column` with the trailing `.*` — the
   executor can detect this pattern.

---

End of document.
