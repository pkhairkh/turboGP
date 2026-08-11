//! The SQL surface — the topmost layer of the engine.
//!
//! turboGP exposes a SQL query language with seven hardware-aware
//! extensions (ADR-015 approximate aggregation, ADR-010 tier pinning, etc.).
//! This module provides the tokenizer, the recursive-descent parser for
//! standard `SELECT`, the extension scanner, and the lowering pass that
//! turns a parsed query into a LogicalPlan.
//!
//! ## Pipeline
//!
//! ```text
//!   SQL string
//!       │
//!       ▼  lexer::tokenize
//!   Vec<Token>
//!       │
//!       ├──▶ parser::parse        ──▶ SelectQuery
//!       │
//!       └──▶ extensions::parse    ──▶ QueryExtensions
//!
//!   (SelectQuery, QueryExtensions)
//!       │
//!       ▼  plan::build_plan
//!   LogicalPlan
//! ```
//!
//! ## Why no external parser crate
//!
//! The spec mandates a hand-written parser so the engine has no external
//! dependency on `sqlparser-rs` or similar. This keeps the build fast and
//! the surface area auditable; it also lets the parser grow the seven
//! turboGP-specific extensions natively rather than via visitor patterns.

pub mod ast;
pub mod cte;
pub mod ddl;
pub mod dml;
pub mod extensions;
pub mod lexer;
pub mod parser;
pub mod pivot;

pub use cte::{parse_with, CteDef, WithClause};
pub use ddl::{
    parse_ddl, AlterAction, AlterTable, ColumnDef, ColumnType, CreateIndex, CreateTable,
    DdlStatement, DropIndex, DropTable,
};
pub use dml::{parse_dml, Delete, DmlStatement, Insert, Update};
pub use extensions::{parse_extensions, parse_extensions_and_strip, QueryExtensions};
pub use lexer::{tokenize, Token, KEYWORDS};
pub use parser::{parse, SelectItem, SelectQuery};
// Unified AST types — re-exported from `ast` so consumers can write
// `turbogp::sql::Expr` directly. The `parser::Expr` re-export still works
// for legacy call sites (`use crate::sql::parser::Expr`).
pub use ast::{BinOp, Expr, UnaryOp, Value};

/// Parse a SQL string into a `(SelectQuery, QueryExtensions)` pair.
///
/// This is the convenience entry point for users who want to parse a full
/// SQL query with turboGP extensions in one call. It:
///
/// 1. Tokenizes the input.
/// 2. Parses extensions from the full token stream (and strips them).
/// 3. Parses the stripped stream as a standard `SELECT`.
///
/// # Example
///
/// ```ignore
/// use turbogp::sql::parse_with_extensions;
/// let (q, ext) = parse_with_extensions(
///     "SELECT AVG(price) APPROXIMATE WITHIN 0.01 CONFIDENCE 0.99 FROM sales"
/// ).unwrap();
/// assert!(ext.approximate.is_some());
/// ```
///
/// # Errors
///
/// Returns `Err(String)` if either the extension scan or the SELECT parse
/// fails.
pub fn parse_with_extensions(sql: &str) -> Result<(SelectQuery, QueryExtensions), String> {
    let tokens = tokenize(sql)?;
    let (ext, stripped) = parse_extensions_and_strip(tokens)?;
    let query = parse(stripped)?;
    Ok((query, ext))
}
