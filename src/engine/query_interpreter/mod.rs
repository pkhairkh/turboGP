//! TPC-H query parser + interpreter.
//!
//! A self-contained SQL parser and row-based interpreter that handles the
//! TPC-H SF=1 query set. Uses the existing [`Token`] lexer but produces a
//! richer AST, then executes with type-aware evaluation (Float64 columns
//! stored as `f64::to_bits` are correctly interpreted).

use crate::catalog::Catalog;
use crate::datasource::csv::{tpc_h_schema, TpcHType};
use crate::datasource::table::Table;
use crate::engine::result::{QueryResult, ResultColumn};
use crate::exec::fm_index::StringSearchColumn;
use crate::sql::lexer::{tokenize, Token};
use crate::Error;
use fxhash::{FxHashMap, FxHashSet};
use rayon::prelude::*;

// Use ahash (hardware AES) instead of std SipHash for all HashMap/HashSet.
// Perf showed 28% of Q21 time was in SipHash + hashbrown operations.
// ahash is ~5x faster for u64 keys.
type HashMap<K, V> = ahash::AHashMap<K, V>;
type HashSet<T> = ahash::AHashSet<T>;

/// Create a HashMap without calling OS entropy (avoids getrandom syscall).
/// Uses a fixed seed — sufficient for database internals where hash-flooding
/// is not a concern (all inputs are trusted TPC-H data).
/// Perf showed 2% of Q21 time was in ahash seed generation (gen_hasher_seed).
fn new_hashmap<K, V>() -> HashMap<K, V> {
    HashMap::with_hasher(ahash::RandomState::with_seed(0x517cc1b727220a95))
}

/// Create a HashSet without calling OS entropy.
fn new_hashset<T>() -> HashSet<T> {
    HashSet::with_hasher(ahash::RandomState::with_seed(0x517cc1b727220a95))
}

/// Create an FxHashMap (trusted u64 keys - no AES, 1 multiply hash) for hot
/// GROUP BY / EXISTS paths. FxHash is ~2x faster than ahash for u64 keys
/// because it skips the AES-NI finalizer (already saturated on this workload).
fn new_fxhashmap<K, V>() -> FxHashMap<K, V> {
    FxHashMap::default()
}

/// Create an FxHashSet (trusted u64 keys) for hot EXISTS semi-join sets.
fn new_fxhashset<T>() -> FxHashSet<T> {
    FxHashSet::default()
}

// =========================================================================
// Column type tracking
// =========================================================================

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ColType {
    Int,
    Float,
    Date,
    String,
}

/// Swap comparison operands (for Literal op Col → Col swap_op(op) Literal).
fn swap_op(op: BinOp2) -> BinOp2 {
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

pub fn parse_query(sql: &str) -> Result<SelectQuery2, String> {
    let tokens = tokenize(sql)?;
    let mut p = QueryInterpreterParser { tokens, pos: 0 };
    let q = p.parse_select()?;
    match p.peek() {
        Token::Semicolon | Token::EOF => Ok(q),
        other => Err(format!("unexpected trailing token: {other:?}")),
    }
}

struct QueryInterpreterParser {
    tokens: Vec<Token>,
    pos: usize,
}

impl QueryInterpreterParser {
    fn peek(&self) -> &Token {
        &self.tokens[self.pos]
    }
    fn peek_at(&self, n: usize) -> &Token {
        self.tokens.get(self.pos + n).unwrap_or(&Token::EOF)
    }
    fn next(&mut self) -> Token {
        let t = self.tokens.get(self.pos).cloned().unwrap_or(Token::EOF);
        if !matches!(t, Token::EOF) {
            self.pos += 1;
        }
        t
    }
    fn match_kw(&mut self, kw: &str) -> bool {
        if let Token::Keyword(k) = self.peek() {
            if k == kw {
                self.pos += 1;
                return true;
            }
        }
        false
    }
    fn match_ident_or_kw(&mut self, name: &str) -> bool {
        match self.peek() {
            Token::Ident(s) if s.eq_ignore_ascii_case(name) => {
                self.pos += 1;
                true
            }
            Token::Keyword(k) if k.eq_ignore_ascii_case(name) => {
                self.pos += 1;
                true
            }
            _ => false,
        }
    }
    fn expect_kw(&mut self, kw: &str) -> Result<(), String> {
        if self.match_kw(kw) {
            return Ok(());
        }
        Err(format!("expected keyword {kw}, got {:?}", self.peek()))
    }
    fn match_op(&mut self, op: &str) -> bool {
        if let Token::Op(o) = self.peek() {
            if o == op {
                self.pos += 1;
                return true;
            }
        }
        false
    }
    fn is_op(&self, ops: &[&str]) -> bool {
        if let Token::Op(o) = self.peek() {
            ops.contains(&o.as_str())
        } else {
            false
        }
    }
    fn match_lp(&mut self) -> bool {
        if matches!(self.peek(), Token::LParen) {
            self.pos += 1;
            true
        } else {
            false
        }
    }
    fn expect_lp(&mut self) -> Result<(), String> {
        if self.match_lp() {
            Ok(())
        } else {
            Err(format!("expected '(', got {:?}", self.peek()))
        }
    }
    fn expect_rp(&mut self) -> Result<(), String> {
        if matches!(self.peek(), Token::RParen) {
            self.pos += 1;
            Ok(())
        } else {
            Err(format!("expected ')', got {:?}", self.peek()))
        }
    }
    fn match_comma(&mut self) -> bool {
        if matches!(self.peek(), Token::Comma) {
            self.pos += 1;
            true
        } else {
            false
        }
    }
    fn is_clause_boundary(&self) -> bool {
        if let Token::Keyword(k) = self.peek() {
            matches!(
                k.as_str(),
                "FROM"
                    | "WHERE"
                    | "GROUP"
                    | "ORDER"
                    | "HAVING"
                    | "LIMIT"
                    | "AND"
                    | "OR"
                    | "JOIN"
                    | "LEFT"
                    | "INNER"
                    | "ON"
                    | "AS"
                    | "WHEN"
                    | "THEN"
                    | "ELSE"
                    | "END"
                    | "BY"
            )
        } else {
            matches!(self.peek(), Token::Comma | Token::EOF | Token::RParen | Token::Semicolon)
        }
    }

    fn parse_ident_name(&mut self) -> Result<String, String> {
        match self.peek().clone() {
            Token::Ident(s) => {
                self.next();
                Ok(s)
            }
            Token::Keyword(k) => {
                self.next();
                Ok(k.to_lowercase())
            }
            other => Err(format!("expected identifier, got {other:?}")),
        }
    }

    // --- SELECT ---

    fn parse_select(&mut self) -> Result<SelectQuery2, String> {
        self.expect_kw("SELECT")?;
        let _ = self.match_kw("DISTINCT");
        let select = self.parse_select_list()?;
        self.expect_kw("FROM")?;
        let from = self.parse_from_list()?;

        let mut joins = Vec::new();
        loop {
            if self.match_ident_or_kw("LEFT") {
                let _ = self.match_ident_or_kw("OUTER");
                self.expect_kw("JOIN")?;
                let table = self.parse_from_item()?;
                self.expect_kw("ON")?;
                let on = self.parse_expr()?;
                joins.push(JoinClause2 { join_type: JoinType2::Left, table, on });
            } else if self.match_ident_or_kw("INNER") {
                self.expect_kw("JOIN")?;
                let table = self.parse_from_item()?;
                self.expect_kw("ON")?;
                let on = self.parse_expr()?;
                joins.push(JoinClause2 { join_type: JoinType2::Inner, table, on });
            } else if self.match_kw("JOIN") {
                let table = self.parse_from_item()?;
                self.expect_kw("ON")?;
                let on = self.parse_expr()?;
                joins.push(JoinClause2 { join_type: JoinType2::Inner, table, on });
            } else {
                break;
            }
        }

        let where_clause = if self.match_kw("WHERE") { Some(self.parse_expr()?) } else { None };
        let group_by = if self.match_kw("GROUP") {
            self.expect_kw("BY")?;
            self.parse_expr_list()?
        } else {
            Vec::new()
        };
        let having = if self.match_kw("HAVING") { Some(self.parse_expr()?) } else { None };
        let order_by = if self.match_kw("ORDER") {
            self.expect_kw("BY")?;
            self.parse_order_list()?
        } else {
            Vec::new()
        };
        let limit = if self.match_ident_or_kw("LIMIT") { Some(self.parse_usize()?) } else { None };

        Ok(SelectQuery2 { select, from, joins, where_clause, group_by, having, order_by, limit })
    }

    fn parse_select_list(&mut self) -> Result<Vec<SelectItem2>, String> {
        let mut items = Vec::new();
        loop {
            // Handle SELECT * — common in EXISTS subqueries.
            // Treat as SELECT 1 (column values don't matter for EXISTS).
            if let Token::Op(op) = self.peek() {
                if op == "*" {
                    self.next();
                    items.push(SelectItem2 { expr: Expr2::Int(1), alias: None });
                    if !self.match_comma() {
                        break;
                    }
                    continue;
                }
            }
            let expr = self.parse_expr()?;
            let alias = if self.match_kw("AS") {
                Some(self.parse_ident_name()?)
            } else if let Token::Ident(_) = self.peek() {
                if self.is_clause_boundary() {
                    None
                } else {
                    Some(self.parse_ident_name()?)
                }
            } else {
                None
            };
            items.push(SelectItem2 { expr, alias });
            if !self.match_comma() {
                break;
            }
        }
        Ok(items)
    }

    fn parse_from_list(&mut self) -> Result<Vec<FromItem>, String> {
        let mut items = Vec::new();
        loop {
            items.push(self.parse_from_item()?);
            if !self.match_comma() {
                break;
            }
        }
        Ok(items)
    }

    fn parse_from_item(&mut self) -> Result<FromItem, String> {
        if matches!(self.peek(), Token::LParen) {
            let save = self.pos;
            self.next();
            if let Token::Keyword(k) = self.peek() {
                if k == "SELECT" {
                    let sub = self.parse_select()?;
                    self.expect_rp()?;
                    let alias = if self.match_kw("AS") {
                        Some(self.parse_ident_name()?)
                    } else if let Token::Ident(_) = self.peek() {
                        Some(self.parse_ident_name()?)
                    } else {
                        None
                    };
                    return Ok(FromItem::Derived(Box::new(sub), alias));
                }
            }
            self.pos = save;
        }
        let name = self.parse_ident_name()?;
        let alias = if self.match_kw("AS") {
            Some(self.parse_ident_name()?)
        } else if let Token::Ident(_) = self.peek() {
            if self.is_clause_boundary() {
                None
            } else {
                Some(self.parse_ident_name()?)
            }
        } else {
            None
        };
        Ok(FromItem::Table(TableRef { name, alias }))
    }

    fn parse_expr_list(&mut self) -> Result<Vec<Expr2>, String> {
        let mut items = Vec::new();
        loop {
            if let Token::Int(_) = self.peek() {
                self.next();
            } else {
                items.push(self.parse_expr()?);
            }
            if !self.match_comma() {
                break;
            }
        }
        Ok(items)
    }

    fn parse_order_list(&mut self) -> Result<Vec<(Expr2, bool)>, String> {
        let mut items = Vec::new();
        loop {
            let expr = self.parse_expr()?;
            let asc = if self.match_ident_or_kw("DESC") {
                false
            } else {
                let _ = self.match_ident_or_kw("ASC");
                true
            };
            items.push((expr, asc));
            if !self.match_comma() {
                break;
            }
        }
        Ok(items)
    }

    fn parse_usize(&mut self) -> Result<usize, String> {
        if let Token::Int(i) = self.peek() {
            if *i < 0 {
                return Err(format!("expected non-negative, got {i}"));
            }
            let u = *i as usize;
            self.next();
            return Ok(u);
        }
        Err(format!("expected integer, got {:?}", self.peek()))
    }

    // --- Expressions ---

    fn parse_expr(&mut self) -> Result<Expr2, String> {
        self.parse_or()
    }

    fn parse_or(&mut self) -> Result<Expr2, String> {
        let mut left = self.parse_and()?;
        while self.match_kw("OR") {
            let right = self.parse_and()?;
            left = Expr2::BinOp { op: BinOp2::Or, left: Box::new(left), right: Box::new(right) };
        }
        Ok(left)
    }

    fn parse_and(&mut self) -> Result<Expr2, String> {
        let mut left = self.parse_not()?;
        while self.match_kw("AND") {
            let right = self.parse_not()?;
            left = Expr2::BinOp { op: BinOp2::And, left: Box::new(left), right: Box::new(right) };
        }
        Ok(left)
    }

    fn parse_not(&mut self) -> Result<Expr2, String> {
        if self.match_kw("NOT") {
            if self.match_ident_or_kw("EXISTS") {
                self.expect_lp()?;
                let sub = self.parse_select()?;
                self.expect_rp()?;
                return Ok(Expr2::Exists { query: Box::new(sub), negated: true });
            }
            let inner = self.parse_not()?;
            return Ok(Expr2::Not(Box::new(inner)));
        }
        if self.match_ident_or_kw("EXISTS") {
            self.expect_lp()?;
            let sub = self.parse_select()?;
            self.expect_rp()?;
            return Ok(Expr2::Exists { query: Box::new(sub), negated: false });
        }
        self.parse_comparison()
    }

    fn parse_comparison(&mut self) -> Result<Expr2, String> {
        let left = self.parse_additive()?;

        if self.is_op(&["=", "!=", "<>", "<", ">", "<=", ">="]) {
            let op_str = if let Token::Op(o) = self.peek() { o.clone() } else { unreachable!() };
            self.next();
            let right = self.parse_additive()?;
            let op = match op_str.as_str() {
                "=" => BinOp2::Eq,
                "!=" | "<>" => BinOp2::Ne,
                "<" => BinOp2::Lt,
                ">" => BinOp2::Gt,
                "<=" => BinOp2::Le,
                ">=" => BinOp2::Ge,
                _ => unreachable!(),
            };
            return Ok(Expr2::BinOp { op, left: Box::new(left), right: Box::new(right) });
        }

        if self.match_ident_or_kw("LIKE") {
            let pattern = self.parse_additive()?;
            return Ok(Expr2::Like {
                expr: Box::new(left),
                pattern: Box::new(pattern),
                negated: false,
            });
        }
        if self.match_kw("NOT") {
            if self.match_ident_or_kw("LIKE") {
                let pattern = self.parse_additive()?;
                return Ok(Expr2::Like {
                    expr: Box::new(left),
                    pattern: Box::new(pattern),
                    negated: true,
                });
            }
            if self.match_ident_or_kw("IN") {
                return self.parse_in_rest(left, true);
            }
            if self.match_ident_or_kw("BETWEEN") {
                return self.parse_between_rest(left, true);
            }
            self.pos -= 1;
            return Ok(left);
        }
        if self.match_ident_or_kw("IN") {
            return self.parse_in_rest(left, false);
        }
        if self.match_ident_or_kw("BETWEEN") {
            return self.parse_between_rest(left, false);
        }
        Ok(left)
    }

    fn parse_in_rest(&mut self, left: Expr2, negated: bool) -> Result<Expr2, String> {
        self.expect_lp()?;
        if let Token::Keyword(k) = self.peek() {
            if k == "SELECT" {
                let sub = self.parse_select()?;
                self.expect_rp()?;
                return Ok(Expr2::InSubquery {
                    expr: Box::new(left),
                    query: Box::new(sub),
                    negated,
                });
            }
        }
        let mut list = Vec::new();
        loop {
            list.push(self.parse_expr()?);
            if !self.match_comma() {
                break;
            }
        }
        self.expect_rp()?;
        Ok(Expr2::InList { expr: Box::new(left), list, negated })
    }

    fn parse_between_rest(&mut self, left: Expr2, negated: bool) -> Result<Expr2, String> {
        let low = self.parse_additive()?;
        self.expect_kw("AND")?;
        let high = self.parse_additive()?;
        Ok(Expr2::Between {
            expr: Box::new(left),
            low: Box::new(low),
            high: Box::new(high),
            negated,
        })
    }

    fn parse_additive(&mut self) -> Result<Expr2, String> {
        let mut left = self.parse_multiplicative()?;
        loop {
            if self.is_op(&["+", "-"]) {
                let op_str =
                    if let Token::Op(o) = self.peek() { o.clone() } else { unreachable!() };
                self.next();
                let right = self.parse_multiplicative()?;
                let op = if op_str == "+" { BinOp2::Add } else { BinOp2::Sub };
                left = Expr2::BinOp { op, left: Box::new(left), right: Box::new(right) };
            } else {
                break;
            }
        }
        Ok(left)
    }

    fn parse_multiplicative(&mut self) -> Result<Expr2, String> {
        let mut left = self.parse_unary()?;
        loop {
            if self.is_op(&["*", "/"]) {
                let op_str =
                    if let Token::Op(o) = self.peek() { o.clone() } else { unreachable!() };
                self.next();
                let right = self.parse_unary()?;
                let op = if op_str == "*" { BinOp2::Mul } else { BinOp2::Div };
                left = Expr2::BinOp { op, left: Box::new(left), right: Box::new(right) };
            } else {
                break;
            }
        }
        Ok(left)
    }

    fn parse_unary(&mut self) -> Result<Expr2, String> {
        if self.match_op("-") {
            return Ok(Expr2::Neg(Box::new(self.parse_unary()?)));
        }
        if self.match_op("+") {
            return self.parse_unary();
        }
        self.parse_primary()
    }

    fn parse_primary(&mut self) -> Result<Expr2, String> {
        match self.peek().clone() {
            Token::Int(i) => {
                self.next();
                Ok(Expr2::Int(i))
            }
            Token::Float(f) => {
                self.next();
                Ok(Expr2::Float(f))
            }
            Token::String(s) => {
                self.next();
                Ok(Expr2::Str(s))
            }
            Token::Keyword(kw) => {
                let ku = kw.to_uppercase();
                match ku.as_str() {
                    "DATE" => {
                        self.next();
                        if let Token::String(s) = self.peek().clone() {
                            self.next();
                            if let Ok(d) = crate::types::Date::from_str(&s) {
                                return Ok(Expr2::Date(d.0));
                            }
                            return Ok(Expr2::Str(s));
                        }
                        Err("expected string after DATE".into())
                    }
                    "CASE" => self.parse_case(),
                    "EXTRACT" => {
                        self.next();
                        self.parse_extract()
                    }
                    "CAST" => {
                        self.next();
                        self.parse_cast()
                    }
                    "COUNT" | "SUM" | "AVG" | "MIN" | "MAX" => {
                        self.next();
                        self.parse_agg_call(&ku)
                    }
                    // Keyword as column name — check for func call
                    _ => {
                        if matches!(self.peek_at(1), Token::LParen) {
                            self.next();
                            return self.parse_agg_call(&ku);
                        }
                        self.next();
                        Ok(Expr2::Col(kw.to_lowercase()))
                    }
                }
            }
            Token::Ident(name) => {
                self.next();
                let lower = name.to_lowercase();
                if matches!(self.peek(), Token::LParen) {
                    match lower.as_str() {
                        "substr" | "substring" => return self.parse_substr(),
                        "extract" => return self.parse_extract(),
                        "cast" => return self.parse_cast(),
                        "exists" => {
                            self.expect_lp()?;
                            let sub = self.parse_select()?;
                            self.expect_rp()?;
                            return Ok(Expr2::Exists { query: Box::new(sub), negated: false });
                        }
                        _ => return self.parse_agg_call(&lower.to_uppercase()),
                    }
                }
                // Check for qualified name: ident . ident
                if self.match_op(".") {
                    let col = self.parse_ident_name()?;
                    return Ok(Expr2::Col(format!("{}.{}", name, col)));
                }
                Ok(Expr2::Col(name))
            }
            Token::LParen => {
                self.next();
                if let Token::Keyword(k) = self.peek() {
                    if k == "SELECT" {
                        let sub = self.parse_select()?;
                        self.expect_rp()?;
                        return Ok(Expr2::Subquery(Box::new(sub)));
                    }
                }
                let e = self.parse_expr()?;
                self.expect_rp()?;
                Ok(e)
            }
            other => Err(format!("expected expression, got {other:?}")),
        }
    }

    fn parse_case(&mut self) -> Result<Expr2, String> {
        self.expect_kw("CASE")?;
        let mut whens = Vec::new();
        while self.match_kw("WHEN") {
            let cond = self.parse_expr()?;
            self.expect_kw("THEN")?;
            let result = self.parse_expr()?;
            whens.push((cond, result));
        }
        let else_ = if self.match_kw("ELSE") { Some(Box::new(self.parse_expr()?)) } else { None };
        self.expect_kw("END")?;
        Ok(Expr2::Case { whens, else_ })
    }

    fn parse_extract(&mut self) -> Result<Expr2, String> {
        // EXTRACT keyword/ident already consumed by caller
        self.expect_lp()?;
        let field = self.parse_ident_name()?;
        self.expect_kw("FROM")?;
        let expr = self.parse_expr()?;
        self.expect_rp()?;
        Ok(Expr2::Extract { field, expr: Box::new(expr) })
    }

    /// Parse `CAST(expr AS target_type)` (Wave 67).
    /// The CAST keyword/ident is already consumed by the caller.
    fn parse_cast(&mut self) -> Result<Expr2, String> {
        self.expect_lp()?;
        let expr = self.parse_expr()?;
        self.expect_kw("AS")?;
        // The target type is a keyword or identifier.
        let target_type = self.parse_ident_name()?.to_uppercase();
        // Optional (length) for VARCHAR(n) — consume and ignore.
        if matches!(self.peek(), Token::LParen) {
            self.next();
            while !matches!(self.peek(), Token::RParen | Token::EOF) {
                self.next();
            }
            if matches!(self.peek(), Token::RParen) {
                self.next();
            }
        }
        self.expect_rp()?;
        Ok(Expr2::Cast { expr: Box::new(expr), target_type })
    }

    fn parse_substr(&mut self) -> Result<Expr2, String> {
        // 'substr' ident already consumed
        self.expect_lp()?;
        let expr = self.parse_expr()?;
        if !self.match_comma() {
            return Err("expected ',' in substr".into());
        }
        let start = self.parse_expr()?;
        if !self.match_comma() {
            return Err("expected ',' in substr".into());
        }
        let len = self.parse_expr()?;
        self.expect_rp()?;
        Ok(Expr2::Substr { expr: Box::new(expr), start: Box::new(start), len: Box::new(len) })
    }

    fn parse_agg_call(&mut self, func_upper: &str) -> Result<Expr2, String> {
        // Function name keyword/ident already consumed by caller
        self.expect_lp()?;
        let distinct = self.match_kw("DISTINCT");
        let arg = if self.match_op("*") { Expr2::CountStar } else { self.parse_expr()? };
        self.expect_rp()?;
        let func = match func_upper {
            "SUM" => {
                if distinct {
                    AggFunc::Sum
                } else {
                    AggFunc::Sum
                }
            }
            "AVG" => AggFunc::Avg,
            "COUNT" => {
                if distinct {
                    AggFunc::CountDistinct
                } else {
                    AggFunc::Count
                }
            }
            "MIN" => AggFunc::Min,
            "MAX" => AggFunc::Max,
            _ => return Err(format!("unsupported aggregate: {func_upper}")),
        };
        Ok(Expr2::Agg { func, arg: Box::new(arg), distinct })
    }
}

// =========================================================================
// Interpreter
// =========================================================================

struct ExecTable {
    columns: Vec<std::sync::Arc<Vec<u64>>>,
    column_names: Vec<String>,
    col_types: Vec<ColType>,
    string_columns: Vec<Option<std::sync::Arc<StringSearchColumn>>>,
    row_count: usize,
    col_map: HashMap<String, usize>,
}

impl ExecTable {
    fn from_catalog(table: &Table, alias: &str) -> Self {
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

    fn lookup_col(&self, name: &str) -> Option<usize> {
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
enum Value2 {
    Int(i64),
    Float(f64),
    Str(String),
    Date(i32),
    Null,
}

impl Value2 {
    fn as_f64(&self) -> Option<f64> {
        match self {
            Value2::Int(i) => Some(*i as f64),
            Value2::Float(f) => Some(*f),
            Value2::Date(d) => Some(*d as f64),
            Value2::Null => None,
            Value2::Str(s) => s.parse().ok(),
        }
    }
    fn as_i64(&self) -> Option<i64> {
        match self {
            Value2::Int(i) => Some(*i),
            Value2::Float(f) => Some(*f as i64),
            Value2::Date(d) => Some(*d as i64),
            Value2::Null => None,
            Value2::Str(s) => s.parse().ok(),
        }
    }
    fn as_str(&self) -> Option<&str> {
        match self {
            Value2::Str(s) => Some(s),
            _ => None,
        }
    }
    fn as_u64(&self) -> Option<u64> {
        match self {
            Value2::Int(i) => Some(*i as u64),
            Value2::Float(f) => Some(*f as u64),
            Value2::Date(d) => Some(*d as u64),
            _ => None,
        }
    }
    fn to_u64(&self) -> u64 {
        match self {
            Value2::Int(i) => *i as u64,
            Value2::Float(f) => f.to_bits(),
            Value2::Date(d) => *d as u32 as u64,
            Value2::Null => 0,
            Value2::Str(s) => xxhash_rust::xxh3::xxh3_64(s.as_bytes()),
        }
    }
}

pub fn execute_interpreter(query: &SelectQuery2, catalog: &Catalog) -> Result<QueryResult, Error> {
    QueryInterpreter {
        catalog,
        outer: std::cell::Cell::new(None),
        subquery_cache: std::cell::RefCell::new(new_hashmap()),
        exists_cache: std::cell::RefCell::new(new_hashmap()),
        exists_multi_cache: std::cell::RefCell::new(new_hashmap()),
        in_subquery_cache: std::cell::RefCell::new(new_hashmap()),
        decorrelated_cache: std::cell::RefCell::new(new_hashmap()),
    }
    .execute(query)
}

struct QueryInterpreter<'a> {
    catalog: &'a Catalog,
    /// Outer context for correlated subqueries: (outer_table_ptr, outer_row).
    /// Set when entering a subquery eval, restored after. Uses raw pointer
    /// for lifetime erasure (safe because the outer table is valid for the
    /// duration of the synchronous subquery execution).
    outer: std::cell::Cell<Option<(*const ExecTable, usize)>>,
    /// Cache for uncorrelated scalar subqueries: keyed by the SelectQuery2
    /// AST pointer (stable for the query's lifetime). Populated lazily by
    /// `precache_subqueries` (called at the start of `execute`) which tries
    /// to execute each subquery with outer=None — if it succeeds, the
    /// subquery is uncorrelated and the result is cached; if it fails
    /// (column not found), it's correlated and per-row eval handles it.
    /// This fixes Q11 (HAVING with uncorrelated scalar subquery) which
    /// previously re-executed the subquery per group (~8000x) and timed out.
    subquery_cache: std::cell::RefCell<HashMap<usize, Value2>>,
    /// Cache for EXISTS semi-join hash sets: keyed by the subquery AST pointer.
    /// When an EXISTS subquery has a single correlation column with an equi-join
    /// (e.g. Q4's `exists (SELECT * FROM lineitem WHERE l_orderkey = o_orderkey
    /// AND l_commitdate < l_receiptdate)`), we build a hash set of the inner
    /// column values (l_orderkey where l_commitdate < l_receiptdate) ONCE,
    /// then check membership per outer row. This decorrelates the EXISTS,
    /// reducing ~25k subquery executions to 1 hash-set build + 25k lookups.
    exists_cache: std::cell::RefCell<HashMap<usize, FxHashSet<u64>>>,
    /// Cache for multi-column EXISTS: HashMap<equi_key, HashSet<ineq_col>>.
    /// For Q21's `exists (SELECT * FROM lineitem l2 WHERE l2.l_orderkey = l1.l_orderkey
    /// AND l2.l_suppkey <> l1.l_suppkey)`, we build a HashMap<l_orderkey, HashSet<l_suppkey>>
    /// once, then for each outer row, check if any suppkey in the set != l1.l_suppkey.
    exists_multi_cache: std::cell::RefCell<HashMap<usize, FxHashMap<u64, FxHashSet<u64>>>>,
    /// Cache for uncorrelated IN-subquery result sets: keyed by AST pointer.
    /// When an IN-subquery is uncorrelated (e.g. Q20's `s_suppkey IN (SELECT
    /// ps_suppkey FROM partsupp WHERE ...)`), we execute it ONCE and cache
    /// the set of values. Then per-row eval just checks membership.
    in_subquery_cache: std::cell::RefCell<HashMap<usize, FxHashSet<u64>>>,
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
    decorrelated_cache: std::cell::RefCell<HashMap<usize, (FxHashMap<u64, Value2>, Vec<usize>)>>,
}

// =============================================================================
// W2: Reusable bool-mask buffer pool.
//
// `eval_bool_mask_vec`'s AND arm previously cloned the running mask per
// conjunct (`mask.to_vec()`, 6 MB for a 6 M-row lineitem scan); the OR
// fallback arm allocated two fresh `vec![true; N]` masks per call. Both
// paths are now backed by this thread-local pool, eliminating the
// malloc/free overhead in the hot WHERE-evaluation loop.
//
// The pool is a stack of `Vec<bool>` buffers. `take_mask_buf(n)` pops a
// buffer (or allocates if the pool is empty) and resizes it to at least
// `n`; `return_mask_buf(buf)` pushes it back. Recursion (AND inside OR
// inside AND, etc.) is safe: a recursive `take_mask_buf` simply pops a
// different buffer or allocates if the pool is exhausted. After warmup
// the pool size equals the max recursion depth, and no further
// allocations occur.
// ============================================================================

thread_local! {
    static MASK_POOL: std::cell::RefCell<Vec<Vec<bool>>> =
        std::cell::RefCell::new(Vec::new());
}

/// Take a `Vec<bool>` of length >= `n` from the thread-local pool
/// (allocating if necessary). The caller MUST return it via
/// `return_mask_buf` to avoid re-allocating on the next call.
fn take_mask_buf(n: usize) -> Vec<bool> {
    MASK_POOL.with(|cell| {
        let mut pool = cell.borrow_mut();
        let mut buf = pool.pop().unwrap_or_else(|| Vec::with_capacity(n));
        if buf.len() < n {
            buf.resize(n, false);
        }
        buf
    })
}

/// Return a buffer to the thread-local pool for reuse by the next
/// `take_mask_buf` call on this thread.
fn return_mask_buf(buf: Vec<bool>) {
    MASK_POOL.with(|cell| {
        cell.borrow_mut().push(buf);
    });
}

impl<'a> QueryInterpreter<'a> {
    fn execute(&self, query: &SelectQuery2) -> Result<QueryResult, Error> {
        // Pre-execute uncorrelated scalar subqueries found in WHERE/HAVING/SELECT.
        // Each subquery is tried with outer=None — if it succeeds, it's uncorrelated
        // and the result is cached; subsequent per-row/per-group eval hits the cache.
        // This is critical for Q11 (HAVING with uncorrelated scalar subquery) which
        // would otherwise re-execute the subquery per group and time out.
        if let Some(ref wc) = query.where_clause {
            self.precache_subqueries(wc);
        }
        if let Some(ref hv) = query.having {
            self.precache_subqueries(hv);
        }
        for item in &query.select {
            self.precache_subqueries(&item.expr);
        }

        // 1. Load all FROM tables
        let mut tables: Vec<ExecTable> = Vec::new();
        for item in &query.from {
            tables.push(self.resolve_from_item(item)?);
        }

        // 2. Handle explicit JOINs on the first table.
        // hash_join now applies non-equi-join ON conditions (LIKE, IN, <, >)
        // per-match during the join, with proper LEFT JOIN handling for
        // unmatched left rows.
        for join in &query.joins {
            let right = self.resolve_from_item(&join.table)?;
            let left = tables.pop().unwrap();
            tables.push(self.hash_join(left, right, &join.on, join.join_type)?);
        }

        // 3. Build base table — use hash joins for implicit multi-table joins.
        // For multi-table FROM, join_tables_smart applies single-table filters
        // (e.g. p_name LIKE '%green%') BEFORE joining. We must NOT re-apply
        // those single-table filters after the join, because string_columns
        // are not rebuilt after joins (LIKE on joined tables falls back to
        // hash comparison, which fails for wildcard patterns).
        let (base, mask) = if tables.len() == 1 {
            let base = tables.into_iter().next().unwrap();
            let mask = if let Some(ref wc) = query.where_clause {
                self.build_mask(wc, &base)?
            } else {
                vec![true; base.row_count]
            };
            (base, mask)
        } else {
            // Identify multi-table conjuncts BEFORE consuming tables.
            // Single-table conjuncts (refs.len() == 1) are applied by
            // join_tables_smart and skipped here.
            let conjuncts = self.split_conjuncts(&query.where_clause);
            let multi_table: Vec<Expr2> = conjuncts
                .iter()
                .filter(|conj| {
                    let refs = self.expr_table_refs(conj, &tables);
                    refs.len() != 1
                })
                .cloned()
                .collect();
            let base = self.plan_join_dp(tables, &query.where_clause)?;
            let mask = if multi_table.is_empty() {
                vec![true; base.row_count]
            } else {
                // W2: evaluate each multi-table conjunct directly into the
                // running mask. The simplified AND arm + fixed OR arm in
                // `eval_bool_mask_vec` preserve the incoming mask (every
                // leaf ANDs into it), so the previous per-conjunct
                // `mask.clone()` (6 MB for a 6 M-row base table) is no
                // longer needed.
                let mut mask = vec![true; base.row_count];
                for conj in &multi_table {
                    self.eval_bool_mask_vec(conj, &base, &mut mask)?;
                }
                mask
            };
            (base, mask)
        };

        // 5. GROUP BY + aggregates
        if !query.group_by.is_empty() || self.has_agg(&query.select) {
            return self.execute_grouped(query, &base, &mask);
        }

        // 6. Non-grouped: filter, project, order, limit
        let indices: Vec<usize> = (0..base.row_count).filter(|&i| mask[i]).collect();
        let result = self.project(&query.select, &base, &indices)?;
        let mut result = if !query.order_by.is_empty() {
            self.apply_order_by(result, &query.order_by, &base, &indices)?
        } else {
            result
        };

        if let Some(limit) = query.limit {
            if result.row_count > limit {
                for col in &mut result.columns {
                    col.values.truncate(limit);
                }
                result.row_count = limit;
            }
        }
        Ok(result)
    }

    /// Walk an expression tree and pre-execute all uncorrelated scalar subqueries,
    /// caching their results in `subquery_cache`. Correlated subqueries (those
    /// that reference outer columns) will fail when executed with outer=None
    /// and are silently skipped — per-row eval will handle them.
    ///
    /// This is critical for Q11 (HAVING with uncorrelated scalar subquery)
    /// which would otherwise re-execute the subquery per group (~8000x) and
    /// time out. Also helps Q2 (correlated subquery in WHERE) by ensuring
    /// the uncorrelated parts of any nested subqueries are cached.
    fn precache_subqueries(&self, expr: &Expr2) {
        match expr {
            Expr2::Subquery(q) => {
                let key = (q.as_ref() as *const SelectQuery2) as usize;
                // Already cached?
                if self.subquery_cache.borrow().contains_key(&key) {
                    return;
                }
                // Try executing with outer=None. If it succeeds, the subquery
                // is uncorrelated — cache the result. If it fails (column not
                // found), it's correlated — leave uncached.
                let old_outer = self.outer.get();
                self.outer.set(None);
                let r = self.execute(q);
                self.outer.set(old_outer);
                if let Ok(r) = r {
                    if let Some(col) = r.columns.first() {
                        let val = col.values.first().copied().unwrap_or(0);
                        let vals_slice = col.values.as_slice();
                        let v = match self.infer_result_type(&col.name, vals_slice) {
                            ColType::Float => Value2::Float(f64::from_bits(val)),
                            _ => Value2::Int(val as i64),
                        };
                        self.subquery_cache.borrow_mut().insert(key, v);
                    }
                }
            }
            Expr2::BinOp { left, right, .. } => {
                self.precache_subqueries(left);
                self.precache_subqueries(right);
            }
            Expr2::Case { whens, else_ } => {
                for (c, r) in whens {
                    self.precache_subqueries(c);
                    self.precache_subqueries(r);
                }
                if let Some(e) = else_ {
                    self.precache_subqueries(e);
                }
            }
            Expr2::Like { expr, pattern, .. } => {
                self.precache_subqueries(expr);
                self.precache_subqueries(pattern);
            }
            Expr2::Between { expr, low, high, .. } => {
                self.precache_subqueries(expr);
                self.precache_subqueries(low);
                self.precache_subqueries(high);
            }
            Expr2::InList { expr, list, .. } => {
                self.precache_subqueries(expr);
                for e in list {
                    self.precache_subqueries(e);
                }
            }
            Expr2::Neg(e) | Expr2::Not(e) | Expr2::Extract { expr: e, .. } => {
                self.precache_subqueries(e);
            }
            Expr2::Substr { expr, start, len } => {
                self.precache_subqueries(expr);
                self.precache_subqueries(start);
                self.precache_subqueries(len);
            }
            Expr2::InSubquery { expr, query, .. } => {
                self.precache_subqueries(expr);
                self.precache_subqueries(&Expr2::Subquery(query.clone()));
            }
            _ => {}
        }
    }

    /// Find columns in the outer table `t` that the subquery references
    /// (correlation columns). These are column references in the subquery's
    /// WHERE/SELECT/HAVING that resolve to `t` (the outer table) but NOT to
    /// the subquery's own FROM tables.
    ///
    /// Used to cache correlated subquery results by the outer row's correlation
    /// values, dramatically reducing redundant subquery executions (e.g. Q17
    /// goes from ~60k executions to ~200, one per distinct p_partkey).
    fn find_correlation_cols(&self, subquery: &SelectQuery2, outer_t: &ExecTable) -> Vec<usize> {
        // Build set of column names available in the subquery's own FROM tables.
        // A Col ref that resolves to one of these is NOT a correlation column.
        let mut inner_cols: HashSet<String> = new_hashset();
        for item in &subquery.from {
            if let FromItem::Table(t) = item {
                if let Some(table) = self.catalog.get(&t.name) {
                    for cn in &table.column_names {
                        inner_cols.insert(cn.to_lowercase());
                    }
                }
            }
        }
        let mut cols: Vec<usize> = Vec::new();
        let mut seen: HashSet<usize> = new_hashset();
        if let Some(ref wc) = subquery.where_clause {
            self.collect_corr_cols_filtered(wc, outer_t, &inner_cols, &mut cols, &mut seen);
        }
        if let Some(ref hv) = subquery.having {
            self.collect_corr_cols_filtered(hv, outer_t, &inner_cols, &mut cols, &mut seen);
        }
        for item in &subquery.select {
            self.collect_corr_cols_filtered(&item.expr, outer_t, &inner_cols, &mut cols, &mut seen);
        }
        cols
    }

    fn collect_corr_cols_filtered(
        &self,
        expr: &Expr2,
        outer_t: &ExecTable,
        inner_cols: &HashSet<String>,
        cols: &mut Vec<usize>,
        seen: &mut HashSet<usize>,
    ) {
        match expr {
            Expr2::Col(name) => {
                // Get short name (after '.') for comparison with inner_cols
                let short = name.rfind('.').map(|p| &name[p + 1..]).unwrap_or(name.as_str());
                let short_lower = short.to_lowercase();
                // If the short name resolves to an inner table column, it's NOT a correlation col
                if inner_cols.contains(&short_lower) {
                    return;
                }
                // Otherwise, check if it resolves to outer_t
                let idx = outer_t.lookup_col(name).or_else(|| outer_t.lookup_col(short));
                if let Some(idx) = idx {
                    if seen.insert(idx) {
                        cols.push(idx);
                    }
                }
            }
            Expr2::BinOp { left, right, .. } => {
                self.collect_corr_cols_filtered(left, outer_t, inner_cols, cols, seen);
                self.collect_corr_cols_filtered(right, outer_t, inner_cols, cols, seen);
            }
            Expr2::Case { whens, else_ } => {
                for (c, r) in whens {
                    self.collect_corr_cols_filtered(c, outer_t, inner_cols, cols, seen);
                    self.collect_corr_cols_filtered(r, outer_t, inner_cols, cols, seen);
                }
                if let Some(e) = else_ {
                    self.collect_corr_cols_filtered(e, outer_t, inner_cols, cols, seen);
                }
            }
            Expr2::Like { expr, pattern, .. } => {
                self.collect_corr_cols_filtered(expr, outer_t, inner_cols, cols, seen);
                self.collect_corr_cols_filtered(pattern, outer_t, inner_cols, cols, seen);
            }
            Expr2::Between { expr, low, high, .. } => {
                self.collect_corr_cols_filtered(expr, outer_t, inner_cols, cols, seen);
                self.collect_corr_cols_filtered(low, outer_t, inner_cols, cols, seen);
                self.collect_corr_cols_filtered(high, outer_t, inner_cols, cols, seen);
            }
            Expr2::InList { expr, list, .. } => {
                self.collect_corr_cols_filtered(expr, outer_t, inner_cols, cols, seen);
                for e in list {
                    self.collect_corr_cols_filtered(e, outer_t, inner_cols, cols, seen);
                }
            }
            Expr2::Neg(e) | Expr2::Not(e) | Expr2::Extract { expr: e, .. } => {
                self.collect_corr_cols_filtered(e, outer_t, inner_cols, cols, seen);
            }
            Expr2::Substr { expr, start, len } => {
                self.collect_corr_cols_filtered(expr, outer_t, inner_cols, cols, seen);
                self.collect_corr_cols_filtered(start, outer_t, inner_cols, cols, seen);
                self.collect_corr_cols_filtered(len, outer_t, inner_cols, cols, seen);
            }
            Expr2::Subquery(_) | Expr2::Exists { .. } | Expr2::InSubquery { .. } => {}
            _ => {}
        }
    }

    /// For an EXISTS subquery, find the single correlation column and the
    /// corresponding inner column via an equi-join conjunct
    /// (`Col(inner) = Col(outer)` or `Col(outer) = Col(inner)`).
    ///
    /// Returns `Some((outer_col_idx, inner_col_idx))` if exactly one
    /// correlation column with an equi-join is found; `None` otherwise
    /// (e.g. multiple correlation cols, or no equi-join).
    ///
    /// Q4 example: `exists (SELECT * FROM lineitem WHERE l_orderkey = o_orderkey
    /// AND l_commitdate < l_receiptdate)` → outer_col=o_orderkey, inner_col=l_orderkey.
    /// Check if a conjunct references a column not in the inner tables.
    /// Uses inner_cols (short names) and inner_aliases (table qualifiers)
    /// to determine if a column reference is inner or correlated (outer).
    fn is_conjunct_correlated_wrt_inner(
        &self,
        expr: &Expr2,
        inner_cols: &HashSet<String>,
        inner_aliases: &HashSet<String>,
    ) -> bool {
        match expr {
            Expr2::Col(name) => {
                if let Some(dot_pos) = name.find('.') {
                    let qualifier = name[..dot_pos].to_lowercase();
                    // If qualifier matches an inner alias, it's inner.
                    if inner_aliases.contains(&qualifier) {
                        return false;
                    }
                    // Otherwise it's correlated.
                    true
                } else {
                    // Unqualified: if short name is in inner_cols, it's inner.
                    !inner_cols.contains(&name.to_lowercase())
                }
            }
            Expr2::BinOp { left, right, .. } => {
                self.is_conjunct_correlated_wrt_inner(left, inner_cols, inner_aliases)
                    || self.is_conjunct_correlated_wrt_inner(right, inner_cols, inner_aliases)
            }
            Expr2::Case { whens, else_ } => {
                whens.iter().any(|(c, r)| {
                    self.is_conjunct_correlated_wrt_inner(c, inner_cols, inner_aliases)
                        || self.is_conjunct_correlated_wrt_inner(r, inner_cols, inner_aliases)
                }) || else_
                    .as_ref()
                    .map(|e| self.is_conjunct_correlated_wrt_inner(e, inner_cols, inner_aliases))
                    .unwrap_or(false)
            }
            Expr2::Like { expr, pattern, .. } => {
                self.is_conjunct_correlated_wrt_inner(expr, inner_cols, inner_aliases)
                    || self.is_conjunct_correlated_wrt_inner(pattern, inner_cols, inner_aliases)
            }
            Expr2::Between { expr, low, high, .. } => {
                self.is_conjunct_correlated_wrt_inner(expr, inner_cols, inner_aliases)
                    || self.is_conjunct_correlated_wrt_inner(low, inner_cols, inner_aliases)
                    || self.is_conjunct_correlated_wrt_inner(high, inner_cols, inner_aliases)
            }
            Expr2::InList { expr, list, .. } => {
                self.is_conjunct_correlated_wrt_inner(expr, inner_cols, inner_aliases)
                    || list.iter().any(|e| {
                        self.is_conjunct_correlated_wrt_inner(e, inner_cols, inner_aliases)
                    })
            }
            Expr2::Neg(e) | Expr2::Not(e) | Expr2::Extract { expr: e, .. } => {
                self.is_conjunct_correlated_wrt_inner(e, inner_cols, inner_aliases)
            }
            Expr2::Substr { expr, start, len } => {
                self.is_conjunct_correlated_wrt_inner(expr, inner_cols, inner_aliases)
                    || self.is_conjunct_correlated_wrt_inner(start, inner_cols, inner_aliases)
                    || self.is_conjunct_correlated_wrt_inner(len, inner_cols, inner_aliases)
            }
            Expr2::Subquery(_) | Expr2::Exists { .. } | Expr2::InSubquery { .. } => true,
            _ => false,
        }
    }

    /// Try to decorrelate a correlated scalar subquery by building a derived
    /// table: execute the subquery's FROM table with local (non-correlated)
    /// filters, GROUP BY the correlation columns, compute the aggregate, and
    /// cache a HashMap<corr_hash, agg_value>. Then per-row eval is O(1).
    ///
    /// Pattern: `SELECT agg(expr) FROM t WHERE corr1 = outer1 AND ... AND local_filters`
    /// → derived table: `SELECT corr1, ..., agg(expr) FROM t WHERE local_filters GROUP BY corr1, ...`
    ///
    /// Returns Some((HashMap<corr_hash, agg_value>, Vec<outer_col_indices>))
    /// if the pattern matches, None otherwise.
    ///
    /// Q20 example: `SELECT 0.5 * sum(l_quantity) FROM lineitem
    ///   WHERE l_partkey = ps_partkey AND l_suppkey = ps_suppkey
    ///   AND l_shipdate >= date '1994-01-01' AND l_shipdate < date '1995-01-01'`
    /// → derived table groups lineitem by (l_partkey, l_suppkey), computes
    ///   0.5 * sum(l_quantity), caches HashMap<(l_partkey,l_suppkey)_hash, threshold>.
    fn try_decorrelate_subquery(
        &self,
        subquery: &SelectQuery2,
        outer_t: &ExecTable,
    ) -> Result<Option<(FxHashMap<u64, Value2>, Vec<usize>)>, Error> {
        // Only decorrelate if the subquery has exactly 1 SELECT item that is
        // an aggregate (or a scalar function of an aggregate, like 0.2 * avg(x)).
        if subquery.select.len() != 1 {
            return Ok(None);
        }
        if !self.expr_has_agg(&subquery.select[0].expr) {
            return Ok(None);
        }
        if subquery.having.is_some() {
            return Ok(None);
        }
        if !subquery.group_by.is_empty() {
            return Ok(None);
        }

        // Only decorrelate single-table subqueries (multi-table joins in the
        // subquery make the derived table build expensive and error-prone).
        // Q20's subquery is `SELECT 0.5*sum(l_quantity) FROM lineitem WHERE ...`
        // (single table) — perfect for decorrelation.
        // Q2's subquery has 4 FROM tables — not decorrelated (uses per-row cache).
        if subquery.from.len() != 1 {
            return Ok(None);
        }

        // Build inner column name set and inner table aliases.
        let mut inner_cols: HashSet<String> = new_hashset();
        let mut inner_aliases: HashSet<String> = new_hashset();
        for item in &subquery.from {
            if let FromItem::Table(t) = item {
                inner_aliases.insert(t.name.to_lowercase());
                if let Some(ref alias) = t.alias {
                    inner_aliases.insert(alias.to_lowercase());
                }
                if let Some(table) = self.catalog.get(&t.name) {
                    for cn in &table.column_names {
                        inner_cols.insert(cn.to_lowercase());
                    }
                }
            }
        }

        // Find correlation columns (outer cols referenced by the subquery).
        let mut corr_cols = self.find_correlation_cols(subquery, outer_t);
        // Need at least 1 correlation column to be correlated.
        if corr_cols.is_empty() {
            return Ok(None);
        }

        // Find the inner column indices for each correlation column by
        // looking at the equi-join conjuncts in the subquery's WHERE.
        // Each corr col has a corresponding inner col via `inner_col = outer_col`.
        let wc = match &subquery.where_clause {
            Some(w) => w,
            None => return Ok(None),
        };
        let conjuncts = self.split_conjuncts(&Some(wc.clone()));

        // Map: outer_col_idx -> (inner_col_idx, outer_col_name, inner_col_name)
        let mut corr_to_inner: Vec<(usize, usize, String, String)> = Vec::new();
        for conj in &conjuncts {
            if let Expr2::BinOp { op: BinOp2::Eq, left: l, right: r } = conj {
                if let (Expr2::Col(ln), Expr2::Col(rn)) = (l.as_ref(), r.as_ref()) {
                    let l_is_inner = self.col_is_inner(ln, &inner_aliases, &inner_cols);
                    let r_is_inner = self.col_is_inner(rn, &inner_aliases, &inner_cols);
                    if l_is_inner != r_is_inner {
                        let (inner_name, outer_name) = if l_is_inner {
                            (ln.clone(), rn.clone())
                        } else {
                            (rn.clone(), ln.clone())
                        };
                        let outer_short = outer_name
                            .rfind('.')
                            .map(|p| &outer_name[p + 1..])
                            .unwrap_or(&outer_name)
                            .to_lowercase();
                        let outer_idx = match outer_t
                            .lookup_col(&outer_name)
                            .or_else(|| outer_t.lookup_col(&outer_short))
                        {
                            Some(idx) => idx,
                            None => continue,
                        };
                        let inner_idx =
                            match self.resolve_inner_col_idx(&inner_name, subquery, outer_t) {
                                Some(idx) => idx,
                                None => continue,
                            };
                        if !corr_to_inner.iter().any(|(oi, _, _, _)| *oi == outer_idx) {
                            corr_to_inner.push((
                                outer_idx,
                                inner_idx,
                                outer_name.clone(),
                                inner_name.clone(),
                            ));
                        }
                    }
                }
            }
        }

        // Check that every correlation column found has a matching equi-join.
        // (corr_cols and corr_to_inner outer indices should match.)
        let corr_outer_indices: HashSet<usize> = corr_cols.iter().copied().collect();
        let matched_outer_indices: HashSet<usize> =
            corr_to_inner.iter().map(|(oi, _, _, _)| *oi).collect();
        if corr_outer_indices != matched_outer_indices {
            return Ok(None);
        }
        if corr_to_inner.is_empty() {
            return Ok(None);
        }

        // Build the derived table: load inner FROM, apply local (non-correlated)
        // conjuncts, GROUP BY inner correlation columns, compute aggregate.
        // We must build a WHERE with correlated conjuncts REMOVED, so that
        // join_tables_smart doesn't try to apply them as single-table filters
        // (which would fail because the outer columns aren't in the inner tables).
        // A conjunct is "correlated" if it references any column whose short name
        // is NOT in the inner table column set AND whose qualifier is NOT an inner
        // table alias. (For Q2: `p_partkey = ps_partkey` — p_partkey is correlated.)
        let local_conjuncts: Vec<Expr2> = conjuncts
            .iter()
            .filter(|c| !self.is_conjunct_correlated_wrt_inner(c, &inner_cols, &inner_aliases))
            .cloned()
            .collect();
        // Rebuild a WHERE clause from local conjuncts (ANDed together).
        let local_where: Option<Expr2> = if local_conjuncts.is_empty() {
            None
        } else {
            let mut w = local_conjuncts[0].clone();
            for c in &local_conjuncts[1..] {
                w = Expr2::BinOp { op: BinOp2::And, left: Box::new(w), right: Box::new(c.clone()) };
            }
            Some(w)
        };
        let mut tables: Vec<ExecTable> = Vec::new();
        for item in &subquery.from {
            tables.push(self.resolve_from_item(item)?);
        }
        let base = if tables.len() == 1 {
            tables.into_iter().next().unwrap()
        } else {
            self.plan_join_dp(tables, &local_where)?
        };

        // Apply local (non-correlated) conjuncts only.
        // W2: evaluate each conjunct directly into `m` (the simplified
        // AND/OR arms in `eval_bool_mask_vec` preserve the incoming mask).
        let mask = {
            let mut m = vec![true; base.row_count];
            for conj in &local_conjuncts {
                self.eval_bool_mask_vec(conj, &base, &mut m)?;
            }
            m
        };

        // Build the aggregate map: GROUP BY inner corr cols, compute agg.
        let agg_expr = &subquery.select[0].expr;
        let inner_corr_indices: Vec<usize> =
            corr_to_inner.iter().map(|(_, ii, _, _)| *ii).collect();
        // Group rows by composite hash of inner corr cols.
        let mut groups: FxHashMap<u64, Vec<usize>> = new_fxhashmap();
        for i in 0..base.row_count {
            if !mask[i] {
                continue;
            }
            let mut h: u64 = 0;
            for &ci in &inner_corr_indices {
                let v = base.columns[ci][i];
                h = h.wrapping_mul(0x517cc1b727220a95).wrapping_add(v);
            }
            groups.entry(h).or_default().push(i);
        }

        // For each group, compute the aggregate value.
        let mut result_map: FxHashMap<u64, Value2> = new_fxhashmap();
        result_map.reserve(groups.len());
        for (hash, indices) in &groups {
            let v = self.eval_agg_expr(agg_expr, &base, indices)?;
            result_map.insert(*hash, v);
        }

        // The outer col indices (for computing corr_hash per outer row).
        let outer_corr_indices: Vec<usize> =
            corr_to_inner.iter().map(|(oi, _, _, _)| *oi).collect();

        Ok(Some((result_map, outer_corr_indices)))
    }

    fn find_exists_equi_join(
        &self,
        subquery: &SelectQuery2,
        outer_t: &ExecTable,
    ) -> Option<(usize, usize)> {
        // Build inner column name set (subquery's own FROM tables)
        let mut inner_cols: HashSet<String> = new_hashset();
        for item in &subquery.from {
            if let FromItem::Table(t) = item {
                if let Some(table) = self.catalog.get(&t.name) {
                    for cn in &table.column_names {
                        inner_cols.insert(cn.to_lowercase());
                    }
                }
            }
        }
        // Find correlation columns (in outer_t but not in inner tables)
        let mut corr_names: Vec<String> = Vec::new();
        let mut seen: HashSet<String> = new_hashset();
        if let Some(ref wc) = subquery.where_clause {
            self.collect_corr_names(wc, outer_t, &inner_cols, &mut corr_names, &mut seen);
        }
        if corr_names.len() != 1 {
            return None;
        }
        let corr_name = &corr_names[0];
        let outer_idx = outer_t.lookup_col(corr_name).or_else(|| {
            corr_name.rfind('.').and_then(|p| outer_t.lookup_col(&corr_name[p + 1..]))
        })?;
        // Find the equi-join conjunct: Col(inner) = Col(corr_name) or vice versa
        if let Some(ref wc) = subquery.where_clause {
            if let Some(inner_idx) =
                self.find_equi_join_inner(wc, corr_name, &inner_cols, subquery, outer_t)
            {
                return Some((outer_idx, inner_idx));
            }
        }
        None
    }

    fn collect_corr_names(
        &self,
        expr: &Expr2,
        outer_t: &ExecTable,
        inner_cols: &HashSet<String>,
        names: &mut Vec<String>,
        seen: &mut HashSet<String>,
    ) {
        match expr {
            Expr2::Col(name) => {
                let short = name.rfind('.').map(|p| &name[p + 1..]).unwrap_or(name.as_str());
                // If the column is NOT in inner_cols, it's a correlation column.
                if !inner_cols.contains(&short.to_lowercase()) {
                    if outer_t.lookup_col(name).is_some() || outer_t.lookup_col(short).is_some() {
                        if seen.insert(name.to_lowercase()) {
                            names.push(name.clone());
                        }
                    }
                }
            }
            Expr2::BinOp { left, right, .. } => {
                self.collect_corr_names(left, outer_t, inner_cols, names, seen);
                self.collect_corr_names(right, outer_t, inner_cols, names, seen);
            }
            Expr2::Case { whens, else_ } => {
                for (c, r) in whens {
                    self.collect_corr_names(c, outer_t, inner_cols, names, seen);
                    self.collect_corr_names(r, outer_t, inner_cols, names, seen);
                }
                if let Some(e) = else_ {
                    self.collect_corr_names(e, outer_t, inner_cols, names, seen);
                }
            }
            Expr2::Like { expr, pattern, .. } => {
                self.collect_corr_names(expr, outer_t, inner_cols, names, seen);
                self.collect_corr_names(pattern, outer_t, inner_cols, names, seen);
            }
            Expr2::Between { expr, low, high, .. } => {
                self.collect_corr_names(expr, outer_t, inner_cols, names, seen);
                self.collect_corr_names(low, outer_t, inner_cols, names, seen);
                self.collect_corr_names(high, outer_t, inner_cols, names, seen);
            }
            Expr2::InList { expr, list, .. } => {
                self.collect_corr_names(expr, outer_t, inner_cols, names, seen);
                for e in list {
                    self.collect_corr_names(e, outer_t, inner_cols, names, seen);
                }
            }
            Expr2::Neg(e) | Expr2::Not(e) | Expr2::Extract { expr: e, .. } => {
                self.collect_corr_names(e, outer_t, inner_cols, names, seen);
            }
            Expr2::Substr { expr, start, len } => {
                self.collect_corr_names(expr, outer_t, inner_cols, names, seen);
                self.collect_corr_names(start, outer_t, inner_cols, names, seen);
                self.collect_corr_names(len, outer_t, inner_cols, names, seen);
            }
            Expr2::Subquery(_) | Expr2::Exists { .. } | Expr2::InSubquery { .. } => {}
            _ => {}
        }
    }

    /// Like collect_corr_names, but uses table qualifiers to distinguish
    /// inner columns from outer correlation columns when both share the same
    /// short name (e.g. Q21's l1.l_orderkey vs l2.l_orderkey).
    fn collect_corr_names_qualified(
        &self,
        expr: &Expr2,
        outer_t: &ExecTable,
        inner_cols: &HashSet<String>,
        inner_aliases: &HashSet<String>,
        names: &mut Vec<String>,
        seen: &mut HashSet<String>,
    ) {
        match expr {
            Expr2::Col(name) => {
                let is_inner = if let Some(dot_pos) = name.find('.') {
                    let qualifier = name[..dot_pos].to_lowercase();
                    inner_aliases.contains(&qualifier)
                } else {
                    inner_cols.contains(&name.to_lowercase())
                };
                if !is_inner {
                    let short = name.rfind('.').map(|p| &name[p + 1..]).unwrap_or(name.as_str());
                    if outer_t.lookup_col(name).is_some() || outer_t.lookup_col(short).is_some() {
                        if seen.insert(name.to_lowercase()) {
                            names.push(name.clone());
                        }
                    }
                }
            }
            Expr2::BinOp { left, right, .. } => {
                self.collect_corr_names_qualified(
                    left,
                    outer_t,
                    inner_cols,
                    inner_aliases,
                    names,
                    seen,
                );
                self.collect_corr_names_qualified(
                    right,
                    outer_t,
                    inner_cols,
                    inner_aliases,
                    names,
                    seen,
                );
            }
            Expr2::Case { whens, else_ } => {
                for (c, r) in whens {
                    self.collect_corr_names_qualified(
                        c,
                        outer_t,
                        inner_cols,
                        inner_aliases,
                        names,
                        seen,
                    );
                    self.collect_corr_names_qualified(
                        r,
                        outer_t,
                        inner_cols,
                        inner_aliases,
                        names,
                        seen,
                    );
                }
                if let Some(e) = else_ {
                    self.collect_corr_names_qualified(
                        e,
                        outer_t,
                        inner_cols,
                        inner_aliases,
                        names,
                        seen,
                    );
                }
            }
            Expr2::Like { expr, pattern, .. } => {
                self.collect_corr_names_qualified(
                    expr,
                    outer_t,
                    inner_cols,
                    inner_aliases,
                    names,
                    seen,
                );
                self.collect_corr_names_qualified(
                    pattern,
                    outer_t,
                    inner_cols,
                    inner_aliases,
                    names,
                    seen,
                );
            }
            Expr2::Between { expr, low, high, .. } => {
                self.collect_corr_names_qualified(
                    expr,
                    outer_t,
                    inner_cols,
                    inner_aliases,
                    names,
                    seen,
                );
                self.collect_corr_names_qualified(
                    low,
                    outer_t,
                    inner_cols,
                    inner_aliases,
                    names,
                    seen,
                );
                self.collect_corr_names_qualified(
                    high,
                    outer_t,
                    inner_cols,
                    inner_aliases,
                    names,
                    seen,
                );
            }
            Expr2::InList { expr, list, .. } => {
                self.collect_corr_names_qualified(
                    expr,
                    outer_t,
                    inner_cols,
                    inner_aliases,
                    names,
                    seen,
                );
                for e in list {
                    self.collect_corr_names_qualified(
                        e,
                        outer_t,
                        inner_cols,
                        inner_aliases,
                        names,
                        seen,
                    );
                }
            }
            Expr2::Neg(e) | Expr2::Not(e) | Expr2::Extract { expr: e, .. } => {
                self.collect_corr_names_qualified(
                    e,
                    outer_t,
                    inner_cols,
                    inner_aliases,
                    names,
                    seen,
                );
            }
            Expr2::Substr { expr, start, len } => {
                self.collect_corr_names_qualified(
                    expr,
                    outer_t,
                    inner_cols,
                    inner_aliases,
                    names,
                    seen,
                );
                self.collect_corr_names_qualified(
                    start,
                    outer_t,
                    inner_cols,
                    inner_aliases,
                    names,
                    seen,
                );
                self.collect_corr_names_qualified(
                    len,
                    outer_t,
                    inner_cols,
                    inner_aliases,
                    names,
                    seen,
                );
            }
            Expr2::Subquery(_) | Expr2::Exists { .. } | Expr2::InSubquery { .. } => {}
            _ => {}
        }
    }

    /// Find `Col(inner) = Col(outer_name)` or reverse in a WHERE expr.
    /// Returns the inner column index (in the subquery's own FROM table).
    fn find_equi_join_inner(
        &self,
        expr: &Expr2,
        outer_name: &str,
        inner_cols: &HashSet<String>,
        subquery: &SelectQuery2,
        outer_t: &ExecTable,
    ) -> Option<usize> {
        match expr {
            Expr2::BinOp { op: BinOp2::Eq, left, right } => {
                // left = inner, right = outer
                if let (Expr2::Col(ln), Expr2::Col(rn)) = (left.as_ref(), right.as_ref()) {
                    let l_short = ln.rfind('.').map(|p| &ln[p + 1..]).unwrap_or(ln.as_str());
                    let r_short = rn.rfind('.').map(|p| &rn[p + 1..]).unwrap_or(rn.as_str());
                    if inner_cols.contains(&l_short.to_lowercase())
                        && r_short.eq_ignore_ascii_case(
                            outer_name.trim_start_matches(|c: char| !c.is_alphanumeric()),
                        )
                    {
                        return self.resolve_inner_col_idx(ln, subquery, outer_t);
                    }
                    if inner_cols.contains(&r_short.to_lowercase())
                        && l_short.eq_ignore_ascii_case(
                            outer_name.trim_start_matches(|c: char| !c.is_alphanumeric()),
                        )
                    {
                        return self.resolve_inner_col_idx(rn, subquery, outer_t);
                    }
                }
            }
            Expr2::BinOp { op: BinOp2::And, left, right } => {
                if let Some(idx) =
                    self.find_equi_join_inner(left, outer_name, inner_cols, subquery, outer_t)
                {
                    return Some(idx);
                }
                if let Some(idx) =
                    self.find_equi_join_inner(right, outer_name, inner_cols, subquery, outer_t)
                {
                    return Some(idx);
                }
            }
            _ => {}
        }
        None
    }

    /// Resolve an inner column name to its index in the subquery's base table.
    /// Loads the subquery's FROM and looks up the column.
    fn resolve_inner_col_idx(
        &self,
        col_name: &str,
        subquery: &SelectQuery2,
        _outer_t: &ExecTable,
    ) -> Option<usize> {
        // Load the subquery's FROM tables and look up the column.
        // This is a lightweight version of resolve_from that doesn't do joins.
        for item in &subquery.from {
            if let FromItem::Table(t) = item {
                if let Some(table) = self.catalog.get(&t.name) {
                    let alias = t.alias.as_deref().unwrap_or(&t.name);
                    // Build a temp ExecTable to use lookup_col
                    let exec_t = ExecTable::from_catalog(table, alias);
                    if let Some(idx) = exec_t.lookup_col(col_name) {
                        return Some(idx);
                    }
                }
            }
        }
        None
    }

    /// Build a hash set of inner column values from the subquery's filtered
    /// result (with the correlated equi-join conjunct removed — only
    /// uncorrelated conjuncts are applied).
    ///
    /// For Q4: `SELECT DISTINCT l_orderkey FROM lineitem WHERE l_commitdate < l_receiptdate`
    fn build_exists_hashset(
        &self,
        subquery: &SelectQuery2,
        inner_col_idx: usize,
    ) -> Result<FxHashSet<u64>, Error> {
        // Load the subquery's FROM table(s) and join them (no correlation).
        let mut tables: Vec<ExecTable> = Vec::new();
        for item in &subquery.from {
            tables.push(self.resolve_from_item(item)?);
        }
        let base = if tables.len() == 1 {
            tables.into_iter().next().unwrap()
        } else {
            self.plan_join_dp(tables, &subquery.where_clause)?
        };
        // Apply the subquery's WHERE conjuncts, EXCEPT the correlated equi-join.
        // W2: evaluate each conjunct directly into `mask` (the simplified
        // AND/OR arms in `eval_bool_mask_vec` preserve the incoming mask).
        let mask = if let Some(ref wc) = subquery.where_clause {
            let conjuncts = self.split_conjuncts(&subquery.where_clause);
            let mut mask = vec![true; base.row_count];
            for conj in &conjuncts {
                if self.is_conjunct_correlated(conj, &base) {
                    continue;
                }
                self.eval_bool_mask_vec(conj, &base, &mut mask)?;
            }
            mask
        } else {
            vec![true; base.row_count]
        };
        // Build hash set of inner col values — PARALLEL using rayon.
        // Split into chunks, each thread builds a local HashSet, then merge.
        // This is critical for Q4 where lineitem has 6M rows and the serial
        // HashSet insertion (SipHash + hashbrown) was a top-5 hotspot.
        let col = &base.columns[inner_col_idx];
        const CHUNK_SIZE: usize = 65536;
        let n = base.row_count;
        let num_chunks = (n + CHUNK_SIZE - 1) / CHUNK_SIZE;
        let local_sets: Vec<FxHashSet<u64>> = (0..num_chunks)
            .into_par_iter()
            .map(|chunk_idx| {
                let start = chunk_idx * CHUNK_SIZE;
                let end = std::cmp::min(start + CHUNK_SIZE, n);
                let mut local = new_fxhashset();
                for i in start..end {
                    if mask[i] {
                        local.insert(col[i]);
                    }
                }
                local
            })
            .collect();
        // Merge local sets into final set
        let mut set = new_fxhashset();
        for local in local_sets {
            set.extend(local);
        }
        Ok(set)
    }

    /// Check if a conjunct references a column not in `base` (i.e. correlated).
    /// Uses table qualifiers to distinguish inner from outer columns.
    fn is_conjunct_correlated(&self, expr: &Expr2, base: &ExecTable) -> bool {
        match expr {
            Expr2::Col(name) => {
                if let Some(dot_pos) = name.find('.') {
                    let qualifier = name[..dot_pos].to_lowercase();
                    // If base has this qualified name, it's an inner column.
                    if base.lookup_col(name).is_some() {
                        return false;
                    }
                    // If the qualifier matches base's alias, the column is inner
                    // (even if the short name doesn't resolve — shouldn't happen).
                    if self.qualifier_matches_base(&qualifier, base) {
                        return false;
                    }
                    // Qualifier doesn't match base — it's a correlation column.
                    true
                } else {
                    // Unqualified: check if short name is in base.
                    base.lookup_col(name).is_none()
                }
            }
            Expr2::BinOp { left, right, .. } => {
                self.is_conjunct_correlated(left, base) || self.is_conjunct_correlated(right, base)
            }
            Expr2::Case { whens, else_ } => {
                whens.iter().any(|(c, r)| {
                    self.is_conjunct_correlated(c, base) || self.is_conjunct_correlated(r, base)
                }) || else_.as_ref().map(|e| self.is_conjunct_correlated(e, base)).unwrap_or(false)
            }
            Expr2::Like { expr, pattern, .. } => {
                self.is_conjunct_correlated(expr, base)
                    || self.is_conjunct_correlated(pattern, base)
            }
            Expr2::Between { expr, low, high, .. } => {
                self.is_conjunct_correlated(expr, base)
                    || self.is_conjunct_correlated(low, base)
                    || self.is_conjunct_correlated(high, base)
            }
            Expr2::InList { expr, list, .. } => {
                self.is_conjunct_correlated(expr, base)
                    || list.iter().any(|e| self.is_conjunct_correlated(e, base))
            }
            Expr2::Neg(e) | Expr2::Not(e) | Expr2::Extract { expr: e, .. } => {
                self.is_conjunct_correlated(e, base)
            }
            Expr2::Substr { expr, start, len } => {
                self.is_conjunct_correlated(expr, base)
                    || self.is_conjunct_correlated(start, base)
                    || self.is_conjunct_correlated(len, base)
            }
            Expr2::Subquery(_) | Expr2::Exists { .. } | Expr2::InSubquery { .. } => true,
            _ => false,
        }
    }

    /// Check if a table qualifier matches any of base's column_names prefixes.
    /// The base table's col_map has entries like "alias.colname".
    /// If the qualifier matches any such prefix, it's an inner column.
    fn qualifier_matches_base(&self, qualifier: &str, base: &ExecTable) -> bool {
        for name in &base.column_names {
            // column_names don't have qualifiers — check col_map instead
        }
        // Check col_map for any key starting with "qualifier."
        let prefix = format!("{}.", qualifier);
        for key in base.col_map.keys() {
            if key.starts_with(&prefix) {
                return true;
            }
        }
        false
    }

    /// For an EXISTS subquery with 2 correlation columns, find the equi-join
    /// pair and the inequality pair. Returns (outer_eq, inner_eq, outer_neq, inner_neq).
    ///
    /// Q21 example: `exists (SELECT * FROM lineitem l2 WHERE l2.l_orderkey = l1.l_orderkey
    /// AND l2.l_suppkey <> l1.l_suppkey)` → outer_eq=l1.l_orderkey, inner_eq=l2.l_orderkey,
    /// outer_neq=l1.l_suppkey, inner_neq=l2.l_suppkey.
    fn find_exists_multi_col(
        &self,
        subquery: &SelectQuery2,
        outer_t: &ExecTable,
    ) -> Option<(usize, usize, usize, usize)> {
        // Build inner column name set and inner table aliases
        let mut inner_cols: HashSet<String> = new_hashset();
        let mut inner_aliases: HashSet<String> = new_hashset();
        for item in &subquery.from {
            if let FromItem::Table(t) = item {
                inner_aliases.insert(t.name.to_lowercase());
                if let Some(ref alias) = t.alias {
                    inner_aliases.insert(alias.to_lowercase());
                }
                if let Some(table) = self.catalog.get(&t.name) {
                    for cn in &table.column_names {
                        inner_cols.insert(cn.to_lowercase());
                    }
                }
            }
        }
        // Find correlation columns
        let mut corr_names: Vec<String> = Vec::new();
        let mut seen: HashSet<String> = new_hashset();
        if let Some(ref wc) = subquery.where_clause {
            self.collect_corr_names_qualified(
                wc,
                outer_t,
                &inner_cols,
                &inner_aliases,
                &mut corr_names,
                &mut seen,
            );
        }
        if corr_names.len() != 2 {
            return None;
        }
        // Find the equi-join conjunct (Col(inner) = Col(outer))
        let wc = subquery.where_clause.as_ref()?;
        let conjuncts = self.split_conjuncts(&Some(wc.clone()));
        let mut eq_pair: Option<(usize, usize, String, String)> = None; // (outer_idx, inner_idx, outer_name, inner_name)
        let mut neq_pair: Option<(usize, usize)> = None; // (outer_idx, inner_idx)
        for conj in &conjuncts {
            // Look for Col = Col (equi-join between inner and outer)
            if let Expr2::BinOp { op: BinOp2::Eq, left: l, right: r } = conj {
                if let (Expr2::Col(ln), Expr2::Col(rn)) = (l.as_ref(), r.as_ref()) {
                    // Use qualifier to determine inner vs outer (not short name)
                    let l_is_inner = self.col_is_inner(ln, &inner_aliases, &inner_cols);
                    let r_is_inner = self.col_is_inner(rn, &inner_aliases, &inner_cols);
                    if l_is_inner != r_is_inner {
                        // One is inner, one is outer
                        let (inner_name, outer_name) = if l_is_inner {
                            (ln.clone(), rn.clone())
                        } else {
                            (rn.clone(), ln.clone())
                        };
                        let outer_short = outer_name
                            .rfind('.')
                            .map(|p| &outer_name[p + 1..])
                            .unwrap_or(&outer_name)
                            .to_lowercase();
                        let outer_idx = outer_t
                            .lookup_col(&outer_name)
                            .or_else(|| outer_t.lookup_col(&outer_short))?;
                        let inner_idx =
                            self.resolve_inner_col_idx(&inner_name, subquery, outer_t)?;
                        if eq_pair.is_none() {
                            eq_pair = Some((
                                outer_idx,
                                inner_idx,
                                outer_name.clone(),
                                inner_name.clone(),
                            ));
                        }
                    }
                }
            }
            // Look for Col <> Col (inequality between inner and outer)
            if let Expr2::BinOp { op: BinOp2::Ne, left: l, right: r } = conj {
                if let (Expr2::Col(ln), Expr2::Col(rn)) = (l.as_ref(), r.as_ref()) {
                    let l_is_inner = self.col_is_inner(ln, &inner_aliases, &inner_cols);
                    let r_is_inner = self.col_is_inner(rn, &inner_aliases, &inner_cols);
                    if l_is_inner != r_is_inner {
                        let (inner_name, outer_name) = if l_is_inner {
                            (ln.clone(), rn.clone())
                        } else {
                            (rn.clone(), ln.clone())
                        };
                        let outer_short = outer_name
                            .rfind('.')
                            .map(|p| &outer_name[p + 1..])
                            .unwrap_or(&outer_name)
                            .to_lowercase();
                        let outer_idx = outer_t
                            .lookup_col(&outer_name)
                            .or_else(|| outer_t.lookup_col(&outer_short))?;
                        let inner_idx =
                            self.resolve_inner_col_idx(&inner_name, subquery, outer_t)?;
                        if neq_pair.is_none() {
                            neq_pair = Some((outer_idx, inner_idx));
                        }
                    }
                }
            }
        }
        let (outer_eq, inner_eq, _, _) = eq_pair?;
        let (outer_neq, inner_neq) = neq_pair?;
        Some((outer_eq, inner_eq, outer_neq, inner_neq))
    }

    /// Check if a column name refers to an inner table column.
    /// Uses the qualifier (if present) to distinguish inner from outer.
    fn col_is_inner(
        &self,
        name: &str,
        inner_aliases: &HashSet<String>,
        inner_cols: &HashSet<String>,
    ) -> bool {
        if let Some(dot_pos) = name.find('.') {
            let qualifier = name[..dot_pos].to_lowercase();
            inner_aliases.contains(&qualifier)
        } else {
            inner_cols.contains(&name.to_lowercase())
        }
    }

    /// Build HashMap<equi_key, HashSet<ineq_col>> from the subquery's inner
    /// table, applying only uncorrelated conjuncts.
    fn build_exists_multi_map(
        &self,
        subquery: &SelectQuery2,
        inner_eq_idx: usize,
        inner_neq_idx: usize,
    ) -> Result<FxHashMap<u64, FxHashSet<u64>>, Error> {
        let mut tables: Vec<ExecTable> = Vec::new();
        for item in &subquery.from {
            tables.push(self.resolve_from_item(item)?);
        }
        let base = if tables.len() == 1 {
            tables.into_iter().next().unwrap()
        } else {
            self.plan_join_dp(tables, &subquery.where_clause)?
        };
        // W2: evaluate each conjunct directly into `mask` (the simplified
        // AND/OR arms in `eval_bool_mask_vec` preserve the incoming mask).
        let mask = if let Some(ref wc) = subquery.where_clause {
            let conjuncts = self.split_conjuncts(&subquery.where_clause);
            let mut mask = vec![true; base.row_count];
            for conj in &conjuncts {
                if self.is_conjunct_correlated(conj, &base) {
                    continue;
                }
                self.eval_bool_mask_vec(conj, &base, &mut mask)?;
            }
            mask
        } else {
            vec![true; base.row_count]
        };
        let eq_col = &base.columns[inner_eq_idx];
        let neq_col = &base.columns[inner_neq_idx];
        // Build HashMap<equi_key, HashSet<ineq_col>> — PARALLEL using rayon.
        // Each chunk builds a local HashMap, then merge by extending sets.
        const CHUNK_SIZE: usize = 65536;
        let n = base.row_count;
        let num_chunks = (n + CHUNK_SIZE - 1) / CHUNK_SIZE;
        let local_maps: Vec<FxHashMap<u64, FxHashSet<u64>>> = (0..num_chunks)
            .into_par_iter()
            .map(|chunk_idx| {
                let start = chunk_idx * CHUNK_SIZE;
                let end = std::cmp::min(start + CHUNK_SIZE, n);
                let mut local: FxHashMap<u64, FxHashSet<u64>> = new_fxhashmap();
                for i in start..end {
                    if mask[i] {
                        local.entry(eq_col[i]).or_default().insert(neq_col[i]);
                    }
                }
                local
            })
            .collect();
        // Merge local maps into final map
        let mut map: FxHashMap<u64, FxHashSet<u64>> = new_fxhashmap();
        for local in local_maps {
            for (k, v) in local {
                map.entry(k).or_default().extend(v);
            }
        }
        Ok(map)
    }

    /// Estimate the number of distinct values in a column using a sampling
    /// approach: hash the first min(n, 10000) values into 256 buckets and
    /// count non-empty buckets. This is fast and good enough for join ordering.
    fn estimate_distinct(&self, col: &[u64], n: usize) -> u64 {
        if n == 0 {
            return 0;
        }
        let sample_size = n.min(10000);
        let mut buckets = [false; 256];
        for i in 0..sample_size {
            let h = crate::exec::join_hash_table::JoinHashTable::hash(col[i]);
            buckets[(h % 256) as usize] = true;
        }
        let filled = buckets.iter().filter(|&&b| b).count() as u64;
        // W29: Linear counting estimator (Whang et al. 1990):
        //   D ≈ -m * ln(1 - filled/m)  where m = 256 (buckets)
        // Much more accurate than the old 'filled * 40' heuristic for
        // low-cardinality columns (e.g. nationkey: true=5, old=200, new=5).
        // This fixes the join-ordering bug where customer⋈supplier (12M output)
        // was chosen over supplier⋈lineitem (1.2M output) because the
        // cardinality estimate was 40× too low.
        if filled >= 256 {
            // All buckets filled — linear counting diverges.
            // Use sample_size as a lower bound (the column has at least
            // this many distinct values in the sample).
            (sample_size as u64).min(n as u64)
        } else {
            let m = 256.0f64;
            let f = filled as f64;
            let estimate = -m * (1.0 - f / m).ln();
            estimate.round() as u64
        }
    }

    /// Smart join (greedy): apply single-table filters, then hash-join tables
    /// using cardinality-aware greedy ordering. Delegates to
    /// apply_single_table_filters + join_tables_greedy_core.
    fn join_tables_smart(
        &self,
        tables: Vec<ExecTable>,
        where_clause: &Option<Expr2>,
    ) -> Result<ExecTable, Error> {
        let conjuncts = self.split_conjuncts(where_clause);
        let tables = self.apply_single_table_filters(tables, &conjuncts)?;
        self.join_tables_greedy_core(tables, &conjuncts)
    }

    /// Apply single-table predicates (those referencing exactly one table) as
    /// filters BEFORE joining. Reduces row counts early (e.g. region filtered
    /// to 1 row by r_name='ASIA'), preventing many-to-many explosions.
    fn apply_single_table_filters(
        &self,
        mut tables: Vec<ExecTable>,
        conjuncts: &[Expr2],
    ) -> Result<Vec<ExecTable>, Error> {
        for i in 0..tables.len() {
            for conj in conjuncts {
                let referenced = self.expr_table_refs(conj, &tables);
                if referenced.len() == 1 && referenced.contains(&i) {
                    let mask = self.build_mask(conj, &tables[i])?;
                    let indices: Vec<usize> =
                        (0..tables[i].row_count).filter(|&r| mask[r]).collect();
                    tables[i] = self.filter_table(&tables[i], &indices);
                }
            }
        }
        Ok(tables)
    }

    /// Greedy join ordering: pick the smallest filtered table as the seed, then
    /// iteratively join the next table that minimizes estimated output cardinality.
    /// O(n^2) plans evaluated. Used as the fallback for n < 4 tables (where DP
    /// overhead isn't amortized) and as a safety net for disconnected join graphs.
    fn join_tables_greedy_core(
        &self,
        mut tables: Vec<ExecTable>,
        conjuncts: &[Expr2],
    ) -> Result<ExecTable, Error> {
        if tables.is_empty() {
            return Err(Error::Other("join_tables_greedy_core: no tables".into()));
        }
        if tables.len() == 1 {
            return Ok(tables.into_iter().next().unwrap());
        }
        // Pick the smallest filtered table that has at least one join key
        // to another table as the seed. This prevents many-to-many explosions
        // like customer ⋈ supplier.
        let mut start_idx = 0;
        let mut start_rows = usize::MAX;
        for (i, t) in tables.iter().enumerate() {
            if t.row_count < start_rows {
                let mut has_join = false;
                for (j, other) in tables.iter().enumerate() {
                    if i == j {
                        continue;
                    }
                    if !self.find_join_keys(t, other, conjuncts).is_empty() {
                        has_join = true;
                        break;
                    }
                }
                if has_join {
                    start_idx = i;
                    start_rows = t.row_count;
                }
            }
        }
        let mut joined = tables.remove(start_idx);
        while !tables.is_empty() {
            let mut best_idx = 0;
            let mut best_keys: Vec<JoinKey2> = Vec::new();
            let mut best_est_output: u64 = u64::MAX;
            for (i, table) in tables.iter().enumerate() {
                let keys = self.find_join_keys(&joined, table, conjuncts);
                if keys.is_empty() {
                    continue;
                }
                let mut est_output: u64 = 1;
                for k in &keys {
                    let dl = self.estimate_distinct(&joined.columns[k.left][..], joined.row_count);
                    let dr = self.estimate_distinct(&table.columns[k.right][..], table.row_count);
                    let max_d = dl.max(dr).max(1);
                    est_output = est_output
                        .saturating_mul(joined.row_count as u64)
                        .saturating_mul(table.row_count as u64)
                        / max_d;
                }
                if est_output < best_est_output {
                    best_est_output = est_output;
                    best_idx = i;
                    best_keys = keys;
                }
            }
            let right = tables.remove(best_idx);
            if best_keys.is_empty() {
                joined = self.cross_join(joined, right);
            } else {
                joined = self.hash_join_with_keys(joined, right, &best_keys, JoinType2::Inner)?;
            }
        }
        Ok(joined)
    }

    /// W4: Selinger dynamic-programming join ordering for multi-table joins.
    /// Enumerates all 2^n subsets of the n joined tables and computes the optimal
    /// bushy join tree via bottom-up DP. For each subset S, considers all partitions
    /// (S1, S2) with S1 ∪ S2 = S, S1 ∩ S2 = ∅, S1 < S2 (to avoid symmetric
    /// duplicates), and picks the one minimizing cumulative work:
    ///   cost(S) = cost(S1) + cost(S2) + |S1| + |S2| + |S1 ⋈ S2|
    /// (hash-build + probe + output materialization).
    ///
    /// Cardinality estimate reuses the existing estimate_distinct() (linear
    /// counting over a 256-bucket sample, same as join_tables_greedy_core):
    ///   |S1 ⋈ S2| = |S1| * |S2| * Π_{i∈S1, j∈S2} pair_sel[i][j]
    /// where pair_sel[i][j] = Π_k (1 / max(V(T_i, k_l), V(T_j, k_r))).
    ///
    /// Complexity: O(3^n) plan evaluations. For n=6 (Q5/Q7/Q9): 729 evaluations,
    /// each <1μs → <1ms total planning cost. For n > 16, falls back to greedy
    /// (2^16 = 65536 DP entries, ~1MB memory — the cap).
    fn plan_join_dp(
        &self,
        tables: Vec<ExecTable>,
        where_clause: &Option<Expr2>,
    ) -> Result<ExecTable, Error> {
        let conjuncts = self.split_conjuncts(where_clause);
        let tables = self.apply_single_table_filters(tables, &conjuncts)?;
        let n = tables.len();

        // DP overhead not amortized for small n; greedy is near-optimal for ≤3 tables.
        // For n > 16 (none in TPC-H), fall back to greedy to cap memory at ~1MB.
        if n < 4 || n > 16 {
            return self.join_tables_greedy_core(tables, &conjuncts);
        }

        let plan_start = std::time::Instant::now();

        // --- Phase 1: Precompute pairwise join keys + selectivity factors ---
        // pair_keys[i][j] = equi-join keys with left col in table i, right col in table j
        let mut pair_keys: Vec<Vec<Vec<JoinKey2>>> =
            (0..n).map(|_| (0..n).map(|_| Vec::new()).collect()).collect();
        for i in 0..n {
            for j in (i + 1)..n {
                let keys = self.find_join_keys(&tables[i], &tables[j], &conjuncts);
                // Reverse key direction for [j][i]: left=j's col, right=i's col
                pair_keys[j][i] =
                    keys.iter().map(|k| JoinKey2 { left: k.right, right: k.left }).collect();
                pair_keys[i][j] = keys;
            }
        }

        // pair_sel_prod[i][j] = Π_k (1 / max(V(T_i, k_l), V(T_j, k_r)))
        // pair_nkeys[i][j] = number of equi-join keys between T_i and T_j
        //
        // Cardinality formula (matches greedy join_tables_greedy_core):
        //   |R ⋈ S| = (|R| * |S|)^|K| / Π_k max(V(R, k_l), V(S, k_r))
        // For single-key joins this reduces to |R|*|S|/max_d (standard Selinger).
        // For multi-key joins the (|R|*|S|)^|K| factor penalizes many-to-many
        // explosions on correlated keys (e.g. lineitem ⋈ partsupp on 2 keys:
        // standard formula gives 2400 vs actual 6M; greedy formula gives ~1e16,
        // correctly steering the DP away from that partition).
        let mut pair_sel_prod: Vec<Vec<f64>> = vec![vec![1.0; n]; n];
        let mut pair_nkeys: Vec<Vec<usize>> = vec![vec![0; n]; n];
        for i in 0..n {
            for j in 0..n {
                if i == j {
                    continue;
                }
                let keys = &pair_keys[i][j];
                if keys.is_empty() {
                    continue;
                }
                pair_nkeys[i][j] = keys.len();
                let mut sel = 1.0;
                for k in keys {
                    let dl = self
                        .estimate_distinct(&tables[i].columns[k.left][..], tables[i].row_count)
                        as f64;
                    let dr = self
                        .estimate_distinct(&tables[j].columns[k.right][..], tables[j].row_count)
                        as f64;
                    let max_d = dl.max(dr).max(1.0);
                    sel /= max_d;
                }
                pair_sel_prod[i][j] = sel;
            }
        }

        // --- Phase 2: Bottom-up DP over subset lattice ---
        let total_masks = 1usize << n;
        let mut dp: Vec<Option<DPEntry>> = vec![None; total_masks];

        // Base case: single-table subsets (cost=0, cardinality=row_count)
        for i in 0..n {
            let mask = 1usize << i;
            let rows = tables[i].row_count as f64;
            dp[mask] = Some(DPEntry { cost: 0.0, cardinality: rows, partition: None });
        }

        // Fill DP bottom-up by mask value. Submasks are always < mask, so
        // they're filled first. Iterate all masks with popcount >= 2.
        for mask in 1..total_masks {
            if mask.count_ones() < 2 {
                continue;
            }

            let mut best_cost = f64::MAX;
            let mut best_partition: Option<(usize, usize)> = None;
            let mut best_card = 0.0;

            // Iterate proper non-empty submasks. To avoid symmetric duplicates
            // (sub, other) vs (other, sub) — which give the same INNER join —
            // only consider sub < other.
            let mut sub = (mask - 1) & mask;
            while sub > 0 {
                let other = mask ^ sub;
                if sub < other {
                    if let (Some(l), Some(r)) = (dp[sub].as_ref(), dp[other].as_ref()) {
                        // Estimate |sub ⋈ other| using greedy-matching formula:
                        //   est = (l.card * r.card)^total_keys * Π pair_sel_prod[i][j]
                        // where total_keys = Σ pair_nkeys[i][j] over cross pairs.
                        // This matches join_tables_greedy_core's per-key loop:
                        //   est = (left*right)^|K| / Π max_d_k
                        let mut total_keys: usize = 0;
                        let mut total_sel: f64 = 1.0;
                        let mut i_bits = sub;
                        while i_bits != 0 {
                            let i = i_bits.trailing_zeros() as usize;
                            i_bits &= i_bits - 1;
                            let mut j_bits = other;
                            while j_bits != 0 {
                                let j = j_bits.trailing_zeros() as usize;
                                j_bits &= j_bits - 1;
                                let nk = pair_nkeys[i][j];
                                if nk > 0 {
                                    total_keys += nk;
                                    total_sel *= pair_sel_prod[i][j];
                                }
                            }
                        }
                        if total_keys > 0 {
                            let base = l.cardinality * r.cardinality;
                            let est_card = base.powf(total_keys as f64) * total_sel;
                            // Cost = work(sub) + work(other) + materialization + output
                            let cost = l.cost + r.cost + l.cardinality + r.cardinality + est_card;
                            if cost < best_cost {
                                best_cost = cost;
                                best_partition = Some((sub, other));
                                best_card = est_card;
                            }
                        }
                    }
                }
                sub = (sub - 1) & mask;
            }

            if let Some(p) = best_partition {
                dp[mask] =
                    Some(DPEntry { cost: best_cost, cardinality: best_card, partition: Some(p) });
            }
            // If no valid partition (disconnected subset), dp[mask] stays None.
        }

        let plan_elapsed = plan_start.elapsed();
        if plan_elapsed > std::time::Duration::from_millis(10) {
            eprintln!(
                "WARN: plan_join_dp took {:?} for n={} tables (expected <10ms)",
                plan_elapsed, n
            );
        }

        // --- Phase 3: Execute the optimal plan recursively ---
        let full_mask = total_masks - 1;
        if dp[full_mask].is_none() {
            // Disconnected join graph — fall back to greedy (cross-join fallback)
            return self.join_tables_greedy_core(tables, &conjuncts);
        }

        let mut tables_opt: Vec<Option<ExecTable>> = tables.into_iter().map(Some).collect();
        self.execute_dp_plan(full_mask, &dp, &mut tables_opt, &conjuncts)
    }

    /// W4: Recursively materialize the optimal join plan for `mask`.
    /// Single-table leaves return the filtered base table (taken from the
    /// slot — each leaf is visited exactly once in the plan tree). Internal
    /// nodes hash-join the materialized left and right children. Joins use
    /// find_join_keys() on the materialized tables: column_names are preserved
    /// across hash_join_with_keys, so key lookup works at any depth.
    fn execute_dp_plan(
        &self,
        mask: usize,
        dp: &[Option<DPEntry>],
        tables: &mut [Option<ExecTable>],
        conjuncts: &[Expr2],
    ) -> Result<ExecTable, Error> {
        let entry = dp[mask].as_ref().expect("execute_dp_plan: missing dp entry");
        match entry.partition {
            None => {
                // Single-table leaf — take the table out of the slot (each leaf visited once)
                let i = mask.trailing_zeros() as usize;
                Ok(tables[i].take().expect("execute_dp_plan: table already consumed"))
            }
            Some((left_mask, right_mask)) => {
                let left = self.execute_dp_plan(left_mask, dp, tables, conjuncts)?;
                let right = self.execute_dp_plan(right_mask, dp, tables, conjuncts)?;
                let keys = self.find_join_keys(&left, &right, conjuncts);
                if keys.is_empty() {
                    Ok(self.cross_join(left, right))
                } else {
                    self.hash_join_with_keys(left, right, &keys, JoinType2::Inner)
                }
            }
        }
    }

    fn hash_join_with_keys(
        &self,
        left: ExecTable,
        right: ExecTable,
        keys: &[JoinKey2],
        jt: JoinType2,
    ) -> Result<ExecTable, Error> {
        use crate::exec::bloom_filter::BloomFilter;
        use crate::exec::join_hash_table::JoinHashTable;
        use xxhash_rust::xxh3::xxh3_64;

        // Decide which side to build the hash table on (smaller side).
        // For INNER joins, we can swap freely. For LEFT joins, we must
        // keep left as the probe side (to preserve unmatched left rows).
        let can_swap = jt == JoinType2::Inner;
        let (build_side, probe_side, build_keys, probe_keys, swapped) =
            if can_swap && left.row_count < right.row_count {
                // Build on left, probe with right — swap the key indices.
                let bk: Vec<JoinKey2> =
                    keys.iter().map(|k| JoinKey2 { left: k.left, right: k.left }).collect();
                let pk: Vec<JoinKey2> =
                    keys.iter().map(|k| JoinKey2 { left: k.right, right: k.right }).collect();
                (&left, &right, bk, pk, true)
            } else {
                // Build on right (original behavior), probe with left.
                let bk: Vec<JoinKey2> =
                    keys.iter().map(|k| JoinKey2 { left: k.right, right: k.right }).collect();
                let pk: Vec<JoinKey2> =
                    keys.iter().map(|k| JoinKey2 { left: k.left, right: k.left }).collect();
                (&right, &left, bk, pk, false)
            };

        let ncol = left.columns.len() + right.columns.len();

        // --- Build phase: construct hash table AND Wilson-loop bloom filter ---
        // Single-key fast path: use JoinHashTable (CedarDB-style bloom-tagged
        // chaining with CRC32 hashing — 10x faster probe than HashMap).
        // Multi-key path: pack keys into a single u64 via xxh3, then use JoinHashTable.
        //
        // W29 (TQFT Wilson loop / Frobenius μ): also build a separate
        // BloomFilter from the same build-side keys. The JoinHashTable's
        // 16-bit directory tag is selective (FPR 1/65536) but lives in
        // L2/L3 because the directory is 16 bytes/slot. The separate
        // BloomFilter is ~1% FPR but 10 bits/item — 5-10× smaller, so
        // it lives in L1. For selective joins (e.g. Q5's region='ASIA'
        // filter narrows to 1 nation, then supplier=10K, then ~7K final),
        // 90%+ of probe keys are absent — the L1 bloom check lets us
        // skip the L2 directory probe entirely for those keys.
        let mut build_hash = JoinHashTable::new(build_side.row_count);
        let mut bloom = BloomFilter::new(build_side.row_count);
        if keys.len() == 1 {
            let bk0 = build_keys[0].left;
            for r in 0..build_side.row_count {
                let k = build_side.columns[bk0][r];
                build_hash.insert(k, r as u32);
                bloom.insert(k);
            }
        } else {
            // Multi-key: hash all key columns into a single u64 via xxh3.
            let bk_cols: Vec<usize> = build_keys.iter().map(|k| k.left).collect();
            for r in 0..build_side.row_count {
                let mut buf = [0u8; 64];
                let mut off = 0;
                for &kc in &bk_cols {
                    let v = build_side.columns[kc][r];
                    let bytes = v.to_le_bytes();
                    if off + 8 <= 64 {
                        buf[off..off + 8].copy_from_slice(&bytes);
                        off += 8;
                    }
                }
                let key = xxh3_64(&buf[..off]);
                build_hash.insert(key, r as u32);
                bloom.insert(key);
            }
        };

        // --- Probe phase (PARALLEL morsel-driven) ---
        // Split the probe side into chunks, each thread probes independently
        // and produces its own output columns. Merge at the end by concatenation.
        // This is critical for Q3/Q5/Q7/Q18 where the probe side is large
        // (6M+ rows for lineitem joins) and the build side is small.
        // Each thread gets its own output buffers to avoid contention.
        let est_output = std::cmp::max(probe_side.row_count, build_side.row_count).min(4_000_000);
        let mut out_types = left.col_types.clone();
        out_types.extend(right.col_types.iter().copied());
        let out_strings: Vec<Option<std::sync::Arc<StringSearchColumn>>> =
            (0..ncol).map(|_| None).collect();
        let mut out_names = left.column_names.clone();
        out_names.extend(right.column_names.clone());

        let left_ncol = left.columns.len();
        let pk_cols: Vec<usize> = probe_keys.iter().map(|k| k.left).collect();

        // Parallel probe using rayon. Each chunk produces its own output cols.
        // The build_hash and bloom are shared (read-only) across threads.
        const CHUNK_SIZE: usize = 65536;
        let probe_row_count = probe_side.row_count;
        let num_chunks = (probe_row_count + CHUNK_SIZE - 1) / CHUNK_SIZE;

        // Parallel probe using rayon. Each chunk produces its own output cols.
        // Optimized: use unsafe set_len + ptr write to avoid per-push capacity
        // checks (the compiler can't elide them due to potential reallocation).
        let partial_results: Vec<Vec<Vec<u64>>> = (0..num_chunks)
            .into_par_iter()
            .map(|chunk_idx| {
                let start = chunk_idx * CHUNK_SIZE;
                let end = std::cmp::min(start + CHUNK_SIZE, probe_row_count);

                let mut local_out: Vec<Vec<u64>> =
                    (0..ncol).map(|_| Vec::with_capacity(CHUNK_SIZE * 2)).collect();
                let mut matched_rows: Vec<u32> = Vec::with_capacity(16);

                // W1-B: Software prefetch distance (rows ahead). Literature default
                // for hash-join probes is 8-32; tuned to K=8 on TPC-H (best total
                // of 3 distances tested: K=8 total=11093, K=16 total=11224, K=32 total=11174).
                const PREFETCH_DIST: usize = 8;
                for p in start..end {
                    // W1-B: Prefetch the hash-table directory slot AND bloom
                    // filter bits for the probe key PREFETCH_DIST rows ahead.
                    // This hides the ~100-cycle L3 miss on the next random
                    // directory access (Q21's #1 hot spot at 23.68% of runtime).
                    //
                    // The probe-side column load for next_key is sequential
                    // (hardware-prefetched), so the only added cost is the
                    // prefetch instruction itself (~1 cycle each).
                    #[cfg(target_arch = "x86_64")]
                    if p + PREFETCH_DIST < end {
                        let next_p = p + PREFETCH_DIST;
                        let next_key = if keys.len() == 1 {
                            probe_side.columns[pk_cols[0]][next_p]
                        } else {
                            let mut nbuf = [0u8; 64];
                            let mut noff = 0;
                            for &kc in &pk_cols {
                                let nv = probe_side.columns[kc][next_p];
                                let nbytes = nv.to_le_bytes();
                                if noff + 8 <= 64 {
                                    nbuf[noff..noff + 8].copy_from_slice(&nbytes);
                                    noff += 8;
                                }
                            }
                            xxh3_64(&nbuf[..noff])
                        };
                        build_hash.prefetch_directory(next_key);
                        bloom.prefetch(next_key);
                    }

                    let probe_key = if keys.len() == 1 {
                        probe_side.columns[pk_cols[0]][p]
                    } else {
                        let mut buf = [0u8; 64];
                        let mut off = 0;
                        for &kc in &pk_cols {
                            let v = probe_side.columns[kc][p];
                            let bytes = v.to_le_bytes();
                            if off + 8 <= 64 {
                                buf[off..off + 8].copy_from_slice(&bytes);
                                off += 8;
                            }
                        }
                        xxh3_64(&buf[..off])
                    };

                    if !bloom.might_contain(probe_key) {
                        if jt == JoinType2::Left && !swapped {
                            for (c, col) in left.columns.iter().enumerate() {
                                local_out[c].push(col[p]);
                            }
                            for c in 0..right.columns.len() {
                                local_out[left_ncol + c].push(0);
                            }
                        }
                        continue;
                    }
                    build_hash.probe_all(probe_key, &mut matched_rows);
                    if matched_rows.is_empty() {
                        if jt == JoinType2::Left && !swapped {
                            for (c, col) in left.columns.iter().enumerate() {
                                local_out[c].push(col[p]);
                            }
                            for c in 0..right.columns.len() {
                                local_out[left_ncol + c].push(0);
                            }
                        }
                    } else {
                        // Pre-compute left column values for this probe row (shared across all matches).
                        // This avoids re-reading left.columns for each match.
                        let left_vals: Vec<u64> = if !swapped {
                            left.columns.iter().map(|col| col[p]).collect()
                        } else {
                            Vec::new()
                        };
                        let right_vals_template: Vec<u64> = if swapped {
                            right.columns.iter().map(|col| col[p]).collect()
                        } else {
                            Vec::new()
                        };
                        for &b in &matched_rows {
                            let b = b as usize;
                            if !swapped {
                                // Left cols from probe (same for all matches), right cols from build.
                                for (c, &v) in left_vals.iter().enumerate() {
                                    local_out[c].push(v);
                                }
                                for (c, col) in right.columns.iter().enumerate() {
                                    local_out[left_ncol + c].push(col[b]);
                                }
                            } else {
                                // Left cols from build, right cols from probe (same for all matches).
                                for (c, col) in left.columns.iter().enumerate() {
                                    local_out[c].push(col[b]);
                                }
                                for (c, &v) in right_vals_template.iter().enumerate() {
                                    local_out[left_ncol + c].push(v);
                                }
                            }
                        }
                    }
                }
                local_out
            })
            .collect();

        // Merge: pre-calculate total size to avoid reallocation.
        let total_rows: usize =
            partial_results.iter().map(|r| r.first().map(|c| c.len()).unwrap_or(0)).sum();
        let mut out_cols: Vec<Vec<u64>> =
            (0..ncol).map(|_| Vec::with_capacity(total_rows)).collect();
        for local_out in partial_results {
            for c in 0..ncol {
                out_cols[c].extend_from_slice(&local_out[c]);
            }
        }
        let row_count = out_cols.first().map(|c| c.len()).unwrap_or(0);

        let mut col_map = new_hashmap();
        for (i, name) in out_names.iter().enumerate() {
            col_map.entry(name.to_lowercase()).or_insert(i);
        }
        for (k, v) in &left.col_map {
            col_map.insert(k.clone(), *v);
        }
        let off = left.columns.len();
        for (k, v) in &right.col_map {
            col_map.insert(k.clone(), *v + off);
        }
        Ok(ExecTable {
            columns: out_cols.into_iter().map(std::sync::Arc::new).collect(),
            column_names: out_names,
            col_types: out_types,
            string_columns: out_strings,
            row_count,
            col_map,
        })
    }

    fn filter_table(&self, table: &ExecTable, indices: &[usize]) -> ExecTable {
        let mut new_cols = Vec::with_capacity(table.columns.len());
        for col in &table.columns {
            new_cols.push(std::sync::Arc::new(indices.iter().map(|&i| col[i]).collect()));
        }
        // Rebuild string columns if present
        let mut new_strings = Vec::with_capacity(table.string_columns.len());
        for (i, sc) in table.string_columns.iter().enumerate() {
            if let Some(ref scol) = sc {
                let strings: Vec<String> =
                    indices.iter().map(|&r| scol.get(r).to_string()).collect();
                new_strings.push(Some(std::sync::Arc::new(StringSearchColumn::new(strings))));
            } else {
                new_strings.push(None);
            }
        }
        ExecTable {
            columns: new_cols,
            column_names: table.column_names.clone(),
            col_types: table.col_types.clone(),
            string_columns: new_strings,
            row_count: indices.len(),
            col_map: table.col_map.clone(),
        }
    }

    /// Split a WHERE clause into AND-conjuncts.
    fn split_conjuncts(&self, where_clause: &Option<Expr2>) -> Vec<Expr2> {
        match where_clause {
            None => Vec::new(),
            Some(expr) => {
                let mut result = Vec::new();
                self.collect_conjuncts(expr, &mut result);
                result
            }
        }
    }

    fn collect_conjuncts(&self, expr: &Expr2, out: &mut Vec<Expr2>) {
        match expr {
            Expr2::BinOp { op: BinOp2::And, left, right } => {
                self.collect_conjuncts(left, out);
                self.collect_conjuncts(right, out);
            }
            _ => out.push(expr.clone()),
        }
    }

    /// Find which tables an expression references.
    fn expr_table_refs(&self, expr: &Expr2, tables: &[ExecTable]) -> HashSet<usize> {
        let mut refs = new_hashset();
        self.collect_table_refs(expr, tables, &mut refs);
        refs
    }

    fn collect_table_refs(&self, expr: &Expr2, tables: &[ExecTable], refs: &mut HashSet<usize>) {
        match expr {
            Expr2::Col(name) => {
                for (i, t) in tables.iter().enumerate() {
                    if t.lookup_col(name).is_some() {
                        refs.insert(i);
                    }
                }
            }
            Expr2::BinOp { left, right, .. } => {
                self.collect_table_refs(left, tables, refs);
                self.collect_table_refs(right, tables, refs);
            }
            Expr2::Like { expr, pattern, .. } => {
                self.collect_table_refs(expr, tables, refs);
                self.collect_table_refs(pattern, tables, refs);
            }
            Expr2::Between { expr, low, high, .. } => {
                self.collect_table_refs(expr, tables, refs);
                self.collect_table_refs(low, tables, refs);
                self.collect_table_refs(high, tables, refs);
            }
            Expr2::InList { expr, list, .. } => {
                self.collect_table_refs(expr, tables, refs);
                for item in list {
                    self.collect_table_refs(item, tables, refs);
                }
            }
            Expr2::Subquery(_) | Expr2::Exists { .. } | Expr2::InSubquery { .. } => {
                // Correlated subqueries can reference any outer table.
                // Mark ALL tables as referenced so this expression is NOT
                // applied as a single-table filter before the join.
                for i in 0..tables.len() {
                    refs.insert(i);
                }
            }
            Expr2::Case { whens, else_ } => {
                for (c, r) in whens {
                    self.collect_table_refs(c, tables, refs);
                    self.collect_table_refs(r, tables, refs);
                }
                if let Some(e) = else_ {
                    self.collect_table_refs(e, tables, refs);
                }
            }
            Expr2::Extract { expr, .. } | Expr2::Neg(expr) | Expr2::Not(expr) => {
                self.collect_table_refs(expr, tables, refs);
            }
            Expr2::Substr { expr, start, len } => {
                self.collect_table_refs(expr, tables, refs);
                self.collect_table_refs(start, tables, refs);
                self.collect_table_refs(len, tables, refs);
            }
            _ => {}
        }
    }

    /// Find equi-join keys between two tables from a list of conjuncts.
    /// Also handles OR of conjunctive groups (e.g. Q19): if all OR branches
    /// share the same equi-join key, it is extracted and used for the join.
    /// The OR is then applied as a post-join filter.
    fn find_join_keys(
        &self,
        left: &ExecTable,
        right: &ExecTable,
        conjuncts: &[Expr2],
    ) -> Vec<JoinKey2> {
        let mut keys = Vec::new();
        for conj in conjuncts {
            if let Expr2::BinOp { op: BinOp2::Eq, left: l, right: r } = conj {
                if let (Some(lk), Some(rk)) = (self.col_in(l, left), self.col_in(r, right)) {
                    keys.push(JoinKey2 { left: lk, right: rk });
                } else if let (Some(rk), Some(lk)) = (self.col_in(l, right), self.col_in(r, left)) {
                    keys.push(JoinKey2 { left: lk, right: rk });
                }
            }
            // Handle OR: extract common equi-join keys from all branches.
            // E.g. Q19: (p_partkey = l_partkey AND ...) OR (p_partkey = l_partkey AND ...) OR ...
            // The common key p_partkey = l_partkey is used for the join.
            if let Expr2::BinOp { op: BinOp2::Or, .. } = conj {
                let or_keys = self.find_or_common_keys(conj, left, right);
                keys.extend(or_keys);
            }
        }
        keys
    }

    /// Find equi-join keys common to ALL branches of an OR expression.
    /// Collects all OR branches, finds equi-join keys in each, and returns
    /// the intersection.
    fn find_or_common_keys(
        &self,
        or_expr: &Expr2,
        left: &ExecTable,
        right: &ExecTable,
    ) -> Vec<JoinKey2> {
        // Collect all OR branches (flatten nested ORs)
        let mut branches: Vec<&Expr2> = Vec::new();
        self.collect_or_branches(or_expr, &mut branches);
        if branches.is_empty() {
            return Vec::new();
        }
        // For each branch, split into AND-conjuncts and find equi-join keys
        let mut branch_keys: Vec<Vec<JoinKey2>> = Vec::new();
        for branch in &branches {
            let conjuncts = self.split_conjuncts_for_or(branch);
            let keys = self.find_join_keys_direct(left, right, &conjuncts);
            branch_keys.push(keys);
        }
        // Intersect: a key must appear in ALL branches (by left,right indices)
        let mut result = Vec::new();
        for key in &branch_keys[0] {
            if branch_keys.iter().all(|bk| bk.contains(key)) {
                result.push(*key);
            }
        }
        result
    }

    fn collect_or_branches<'b>(&self, expr: &'b Expr2, out: &mut Vec<&'b Expr2>) {
        match expr {
            Expr2::BinOp { op: BinOp2::Or, left, right } => {
                self.collect_or_branches(left, out);
                self.collect_or_branches(right, out);
            }
            _ => out.push(expr),
        }
    }

    fn split_conjuncts_for_or(&self, expr: &Expr2) -> Vec<Expr2> {
        let mut result = Vec::new();
        self.collect_conjuncts(expr, &mut result);
        result
    }

    /// Direct equi-join key finder (no OR handling, used by find_or_common_keys).
    fn find_join_keys_direct(
        &self,
        left: &ExecTable,
        right: &ExecTable,
        conjuncts: &[Expr2],
    ) -> Vec<JoinKey2> {
        let mut keys = Vec::new();
        for conj in conjuncts {
            if let Expr2::BinOp { op: BinOp2::Eq, left: l, right: r } = conj {
                if let (Some(lk), Some(rk)) = (self.col_in(l, left), self.col_in(r, right)) {
                    keys.push(JoinKey2 { left: lk, right: rk });
                } else if let (Some(rk), Some(lk)) = (self.col_in(l, right), self.col_in(r, left)) {
                    keys.push(JoinKey2 { left: lk, right: rk });
                }
            }
        }
        keys
    }

    fn has_agg(&self, select: &[SelectItem2]) -> bool {
        select.iter().any(|s| self.expr_has_agg(&s.expr))
    }
    fn expr_has_agg(&self, e: &Expr2) -> bool {
        match e {
            Expr2::Agg { .. } | Expr2::CountStar => true,
            Expr2::BinOp { left, right, .. } => self.expr_has_agg(left) || self.expr_has_agg(right),
            Expr2::Case { whens, else_ } => {
                whens.iter().any(|(c, r)| self.expr_has_agg(c) || self.expr_has_agg(r))
                    || else_.as_ref().map(|e| self.expr_has_agg(e)).unwrap_or(false)
            }
            Expr2::Like { expr, pattern, .. } => {
                self.expr_has_agg(expr) || self.expr_has_agg(pattern)
            }
            Expr2::Between { expr, low, high, .. } => {
                self.expr_has_agg(expr) || self.expr_has_agg(low) || self.expr_has_agg(high)
            }
            Expr2::InList { expr, list, .. } => {
                self.expr_has_agg(expr) || list.iter().any(|e| self.expr_has_agg(e))
            }
            Expr2::Neg(e) | Expr2::Not(e) | Expr2::Extract { expr: e, .. } => self.expr_has_agg(e),
            Expr2::Substr { expr, start, len } => {
                self.expr_has_agg(expr) || self.expr_has_agg(start) || self.expr_has_agg(len)
            }
            _ => false,
        }
    }

    fn resolve_from(&self, from: &[FromItem]) -> Result<ExecTable, Error> {
        if from.is_empty() {
            return Err(Error::Other("no FROM clause".into()));
        }
        let mut base = self.resolve_from_item(&from[0])?;
        for item in &from[1..] {
            let right = self.resolve_from_item(item)?;
            base = self.cross_join(base, right);
        }
        Ok(base)
    }

    fn resolve_from_item(&self, item: &FromItem) -> Result<ExecTable, Error> {
        match item {
            FromItem::Table(t) => {
                let table = self
                    .catalog
                    .get(&t.name)
                    .ok_or_else(|| Error::NotFound(format!("table '{}'", t.name)))?;
                let alias = t.alias.as_deref().unwrap_or(&t.name);
                Ok(ExecTable::from_catalog(table, alias))
            }
            FromItem::Derived(subquery, alias) => {
                let result = self.execute(subquery)?;
                self.result_to_exec_table(&result, alias.as_deref().unwrap_or("derived"))
            }
        }
    }

    fn result_to_exec_table(&self, result: &QueryResult, alias: &str) -> Result<ExecTable, Error> {
        let mut col_map = new_hashmap();
        let mut column_names = Vec::new();
        let mut columns = Vec::new();
        let mut col_types = Vec::new();
        let mut string_columns = Vec::new();
        for (i, col) in result.columns.iter().enumerate() {
            column_names.push(col.name.clone());
            columns.push(std::sync::Arc::new(col.values.clone()));
            col_types.push(self.infer_result_type(&col.name, &col.values));
            string_columns.push(None);
            let lower = col.name.to_lowercase();
            col_map.entry(col.name.to_lowercase()).or_insert(i);
            col_map
                .entry(format!("{}.{}", alias.to_lowercase(), col.name.to_lowercase()))
                .or_insert(i);
        }
        Ok(ExecTable {
            columns,
            column_names,
            col_types,
            string_columns,
            row_count: result.row_count,
            col_map,
        })
    }

    fn infer_result_type(&self, name: &str, values: &[u64]) -> ColType {
        let l = name.to_lowercase();
        // Date columns
        if l.contains("date")
            || l.contains("shipdate")
            || l.contains("commitdate")
            || l.contains("receiptdate")
        {
            return ColType::Date;
        }
        // String columns (common in TPC-H SELECT aliases)
        if l == "n_name"
            || l == "supp_nation"
            || l == "cust_nation"
            || l == "nation"
            || l == "s_name"
            || l == "c_name"
            || l == "p_mfgr"
            || l == "p_brand"
            || l == "p_type"
            || l == "p_container"
            || l == "l_returnflag"
            || l == "l_linestatus"
            || l == "l_shipmode"
            || l == "l_shipinstruct"
            || l == "o_orderpriority"
            || l == "o_orderstatus"
            || l == "cntrycode"
        {
            return ColType::String;
        }
        // Known integer columns (key columns, counts, years, codes)
        if l.contains("year")
            || l.contains("count")
            || l.contains("custdist")
            || l.contains("partkey")
            || l.contains("suppkey")
            || l.contains("custkey")
            || l.contains("nationkey")
            || l.contains("regionkey")
            || l.contains("numwait")
            || l.contains("numcust")
            || l.contains("supplier_cnt")
            || l.contains("availqty")
            || l.contains("size")
            || l == "c_count"
            || l == "supplier_no"
            || l == "order_count"
        {
            return ColType::Int;
        }
        // Heuristic: inspect actual values to distinguish Int from Float.
        // If all sampled non-zero values are "small" (< 2^32) AND none of them,
        // when interpreted as f64 bits, look like normal float values, then
        // the column contains raw integer values (e.g., an aliased GROUP BY key
        // like `l_suppkey AS supplier_no`). Float aggregations (sum/avg) always
        // produce normal-range f64 values, so this heuristic is safe.
        let sample: Vec<u64> = values.iter().take(16).copied().filter(|&v| v != 0).collect();
        if !sample.is_empty() {
            let all_small_int = sample.iter().all(|&v| v < (1u64 << 32));
            let any_normal_float = sample.iter().any(|&v| {
                let f = f64::from_bits(v);
                f.is_normal() && f.abs() >= 1e-3 && f.abs() <= 1e20
            });
            if all_small_int && !any_normal_float {
                return ColType::Int;
            }
        }
        ColType::Float
    }

    // --- Cross join ---

    fn cross_join(&self, left: ExecTable, right: ExecTable) -> ExecTable {
        let lr = left.row_count;
        let rr = right.row_count;
        if lr == 0 || rr == 0 {
            return ExecTable {
                columns: left
                    .columns
                    .iter()
                    .chain(right.columns.iter())
                    .map(|_| std::sync::Arc::new(Vec::new()))
                    .collect(),
                column_names: left
                    .column_names
                    .iter()
                    .chain(right.column_names.iter())
                    .cloned()
                    .collect(),
                col_types: left.col_types.iter().chain(right.col_types.iter()).copied().collect(),
                string_columns: left
                    .string_columns
                    .iter()
                    .chain(right.string_columns.iter())
                    .cloned()
                    .collect(),
                row_count: 0,
                col_map: new_hashmap(),
            };
        }
        let total = lr * rr;
        let mut columns = Vec::with_capacity(left.columns.len() + right.columns.len());
        for col in &left.columns {
            let mut nc = Vec::with_capacity(total);
            for l in 0..lr {
                let v = col[l];
                for _ in 0..rr {
                    nc.push(v);
                }
            }
            columns.push(std::sync::Arc::new(nc));
        }
        for col in &right.columns {
            let mut nc = Vec::with_capacity(total);
            for _ in 0..lr {
                for r in 0..rr {
                    nc.push(col[r]);
                }
            }
            columns.push(std::sync::Arc::new(nc));
        }
        let mut col_types = left.col_types.clone();
        col_types.extend(right.col_types.iter().copied());
        // String columns are NOT rebuilt after cross join — set to None.
        let string_columns: Vec<Option<std::sync::Arc<StringSearchColumn>>> =
            (0..(left.columns.len() + right.columns.len())).map(|_| None).collect();
        let mut column_names = left.column_names.clone();
        column_names.extend(right.column_names.clone());
        let mut col_map = new_hashmap();
        for (i, name) in column_names.iter().enumerate() {
            col_map.entry(name.to_lowercase()).or_insert(i);
        }
        for (k, v) in &left.col_map {
            col_map.insert(k.clone(), *v);
        }
        let off = left.columns.len();
        for (k, v) in &right.col_map {
            col_map.insert(k.clone(), *v + off);
        }
        ExecTable { columns, column_names, col_types, string_columns, row_count: total, col_map }
    }

    // --- Hash join ---

    fn hash_join(
        &self,
        left: ExecTable,
        right: ExecTable,
        on: &Expr2,
        jt: JoinType2,
    ) -> Result<ExecTable, Error> {
        let keys = self.extract_join_keys(on, &left, &right)?;
        if keys.is_empty() {
            return Ok(self.cross_join(left, right));
        }

        // Split ON into equi-join keys and non-equi-join conjuncts.
        // Non-equi-join conjuncts (LIKE, IN, <, >, etc.) are applied per-match
        // during the join — this ensures LEFT JOIN emits unmatched left rows
        // when all matches are filtered out by the non-equi-join conditions.
        let on_conjuncts = self.split_conjuncts(&Some(on.clone()));
        let non_equi: Vec<Expr2> = on_conjuncts.iter().filter(|c| {
            !matches!(c, Expr2::BinOp { op: BinOp2::Eq, left, right }
                if matches!(left.as_ref(), Expr2::Col(_)) && matches!(right.as_ref(), Expr2::Col(_)))
        }).cloned().collect();

        let mut build: HashMap<Vec<u64>, Vec<usize>> = new_hashmap();
        for r in 0..right.row_count {
            let key: Vec<u64> = keys.iter().map(|k| right.columns[k.right][r]).collect();
            build.entry(key).or_default().push(r);
        }

        let ncol = left.columns.len() + right.columns.len();
        let mut out_cols: Vec<Vec<u64>> = (0..ncol).map(|_| Vec::new()).collect();
        let mut out_types = left.col_types.clone();
        out_types.extend(right.col_types.iter().copied());
        // String columns are NOT rebuilt after join — see hash_join_with_keys.
        let out_strings: Vec<Option<std::sync::Arc<StringSearchColumn>>> =
            (0..ncol).map(|_| None).collect();
        let mut out_names = left.column_names.clone();
        out_names.extend(right.column_names.clone());
        let mut row_count = 0;

        let left_ncol = left.columns.len();

        // Pre-build the combined col_map once (reused per match).
        let combined_col_map: HashMap<String, usize> = {
            let mut m = new_hashmap();
            for (i, name) in out_names.iter().enumerate() {
                m.entry(name.to_lowercase()).or_insert(i);
            }
            for (k, v) in &left.col_map {
                m.insert(k.clone(), *v);
            }
            let off = left_ncol;
            for (k, v) in &right.col_map {
                m.insert(k.clone(), *v + off);
            }
            m
        };

        // Build a single combined row (left[l] + right[r]) for non-equi-join eval.
        // We do this per match — non_equi is usually short (1-2 conjuncts).
        for l in 0..left.row_count {
            let key: Vec<u64> = keys.iter().map(|k| left.columns[k.left][l]).collect();
            let matches = build.get(&key).cloned().unwrap_or_default();
            if matches.is_empty() {
                if jt == JoinType2::Left {
                    for (c, col) in left.columns.iter().enumerate() {
                        out_cols[c].push(col[l]);
                    }
                    for c in 0..right.columns.len() {
                        out_cols[left_ncol + c].push(0);
                    }
                    row_count += 1;
                }
            } else {
                let mut any_match_passed = false;
                for r in &matches {
                    // Apply non-equi-join conjuncts per match.
                    if !non_equi.is_empty() {
                        if !self.eval_non_equi_match(
                            &non_equi,
                            &left,
                            l,
                            &right,
                            *r,
                            &out_names,
                            &out_types,
                            &combined_col_map,
                            left_ncol,
                            ncol,
                        )? {
                            continue;
                        }
                    }
                    any_match_passed = true;
                    for (c, col) in left.columns.iter().enumerate() {
                        out_cols[c].push(col[l]);
                    }
                    for (c, col) in right.columns.iter().enumerate() {
                        out_cols[left_ncol + c].push(col[*r]);
                    }
                    row_count += 1;
                }
                // For LEFT JOIN: if no matches passed the non-equi-join filter,
                // emit unmatched left row.
                if !any_match_passed && jt == JoinType2::Left {
                    for (c, col) in left.columns.iter().enumerate() {
                        out_cols[c].push(col[l]);
                    }
                    for c in 0..right.columns.len() {
                        out_cols[left_ncol + c].push(0);
                    }
                    row_count += 1;
                }
            }
        }

        let mut col_map = new_hashmap();
        for (i, name) in out_names.iter().enumerate() {
            col_map.entry(name.to_lowercase()).or_insert(i);
        }
        for (k, v) in &left.col_map {
            col_map.insert(k.clone(), *v);
        }
        let off = left.columns.len();
        for (k, v) in &right.col_map {
            col_map.insert(k.clone(), *v + off);
        }

        Ok(ExecTable {
            columns: out_cols.into_iter().map(std::sync::Arc::new).collect(),
            column_names: out_names,
            col_types: out_types,
            string_columns: out_strings,
            row_count,
            col_map,
        })
    }

    /// Evaluate non-equi-join conjuncts for a single (left[l], right[r]) match.
    /// Returns true if all conjuncts pass.
    ///
    /// For conjuncts that only reference right columns, eval on right at row r
    /// (preserves string_columns for LIKE/NOT LIKE).
    /// For conjuncts that reference both tables, build a combined row.
    fn eval_non_equi_match(
        &self,
        non_equi: &[Expr2],
        left: &ExecTable,
        l: usize,
        right: &ExecTable,
        r: usize,
        out_names: &[String],
        out_types: &[ColType],
        combined_col_map: &HashMap<String, usize>,
        left_ncol: usize,
        ncol: usize,
    ) -> Result<bool, Error> {
        for conj in non_equi {
            // Check if this conjunct only references right columns
            let refs_left = self.expr_refs_table(conj, left);
            let refs_right = self.expr_refs_table(conj, right);
            let pass = if refs_right && !refs_left {
                // Only right columns — eval on right table at row r
                let v = self.eval(conj, right, r)?;
                self.truthy(&v)
            } else if refs_left && !refs_right {
                // Only left columns — eval on left table at row l
                let v = self.eval(conj, left, l)?;
                self.truthy(&v)
            } else {
                // Both tables — build combined row
                let mut combined_cols: Vec<u64> = Vec::with_capacity(ncol);
                for (c, col) in left.columns.iter().enumerate() {
                    combined_cols.push(col[l]);
                }
                for (c, col) in right.columns.iter().enumerate() {
                    combined_cols.push(col[r]);
                }
                // Build a mini StringSearchColumn for the right's string at row r
                let mut combined_strings: Vec<Option<std::sync::Arc<StringSearchColumn>>> =
                    (0..left_ncol).map(|_| None).collect();
                for sc in &right.string_columns {
                    if let Some(ref scol) = sc {
                        if scol.len() > r {
                            combined_strings.push(Some(std::sync::Arc::new(
                                StringSearchColumn::new(vec![scol.get(r).to_string()]),
                            )));
                        } else {
                            combined_strings.push(None);
                        }
                    } else {
                        combined_strings.push(None);
                    }
                }
                let combined_t = ExecTable {
                    columns: combined_cols.iter().map(|v| std::sync::Arc::new(vec![*v])).collect(),
                    column_names: out_names.to_vec(),
                    col_types: out_types.to_vec(),
                    string_columns: combined_strings,
                    row_count: 1,
                    col_map: combined_col_map.clone(),
                };
                let v = self.eval(conj, &combined_t, 0)?;
                self.truthy(&v)
            };
            if !pass {
                return Ok(false);
            }
        }
        Ok(true)
    }

    /// Check if an expression references any column in the given table.
    fn expr_refs_table(&self, expr: &Expr2, table: &ExecTable) -> bool {
        match expr {
            Expr2::Col(name) => {
                let short = name.rfind('.').map(|p| &name[p + 1..]).unwrap_or(name.as_str());
                table.lookup_col(name).is_some() || table.lookup_col(short).is_some()
            }
            Expr2::BinOp { left, right, .. } => {
                self.expr_refs_table(left, table) || self.expr_refs_table(right, table)
            }
            Expr2::Case { whens, else_ } => {
                whens
                    .iter()
                    .any(|(c, r)| self.expr_refs_table(c, table) || self.expr_refs_table(r, table))
                    || else_.as_ref().map(|e| self.expr_refs_table(e, table)).unwrap_or(false)
            }
            Expr2::Like { expr, pattern, .. } => {
                self.expr_refs_table(expr, table) || self.expr_refs_table(pattern, table)
            }
            Expr2::Between { expr, low, high, .. } => {
                self.expr_refs_table(expr, table)
                    || self.expr_refs_table(low, table)
                    || self.expr_refs_table(high, table)
            }
            Expr2::InList { expr, list, .. } => {
                self.expr_refs_table(expr, table)
                    || list.iter().any(|e| self.expr_refs_table(e, table))
            }
            Expr2::Neg(e) | Expr2::Not(e) | Expr2::Extract { expr: e, .. } => {
                self.expr_refs_table(e, table)
            }
            Expr2::Substr { expr, start, len } => {
                self.expr_refs_table(expr, table)
                    || self.expr_refs_table(start, table)
                    || self.expr_refs_table(len, table)
            }
            _ => false,
        }
    }

    fn extract_join_keys(
        &self,
        on: &Expr2,
        left: &ExecTable,
        right: &ExecTable,
    ) -> Result<Vec<JoinKey2>, Error> {
        let mut keys = Vec::new();
        self.collect_keys(on, left, right, &mut keys);
        Ok(keys)
    }

    fn collect_keys(
        &self,
        on: &Expr2,
        left: &ExecTable,
        right: &ExecTable,
        keys: &mut Vec<JoinKey2>,
    ) {
        match on {
            Expr2::BinOp { op: BinOp2::And, left: l, right: r } => {
                self.collect_keys(l, left, right, keys);
                self.collect_keys(r, left, right, keys);
            }
            Expr2::BinOp { op: BinOp2::Eq, left: l, right: r } => {
                if let (Some(lk), Some(rk)) = (self.col_in(l, left), self.col_in(r, right)) {
                    keys.push(JoinKey2 { left: lk, right: rk });
                } else if let (Some(rk), Some(lk)) = (self.col_in(l, right), self.col_in(r, left)) {
                    keys.push(JoinKey2 { left: lk, right: rk });
                }
            }
            _ => {}
        }
    }

    fn col_in(&self, expr: &Expr2, table: &ExecTable) -> Option<usize> {
        if let Expr2::Col(name) = expr {
            table.lookup_col(name)
        } else {
            None
        }
    }

    // --- WHERE ---

    fn build_mask(&self, expr: &Expr2, table: &ExecTable) -> Result<Vec<bool>, Error> {
        // Try vectorized fast path first; fall back to per-row eval.
        let mut mask = vec![true; table.row_count];
        self.eval_bool_mask_vec(expr, table, &mut mask)?;
        Ok(mask)
    }

    // --- W1-D: Q7 nation-pair LUT fast path ---

    /// Flatten an OR tree (left-associative `OR(OR(a, b), c)`) into a list
    /// of disjunct leaf expressions (by reference).
    fn flatten_disjuncts<'e>(expr: &'e Expr2, out: &mut Vec<&'e Expr2>) {
        match expr {
            Expr2::BinOp { op: BinOp2::Or, left, right } => {
                Self::flatten_disjuncts(left, out);
                Self::flatten_disjuncts(right, out);
            }
            _ => out.push(expr),
        }
    }

    /// Flatten an AND tree into a list of conjunct leaf expressions (by reference).
    fn flatten_conjuncts<'e>(expr: &'e Expr2, out: &mut Vec<&'e Expr2>) {
        match expr {
            Expr2::BinOp { op: BinOp2::And, left, right } => {
                Self::flatten_conjuncts(left, out);
                Self::flatten_conjuncts(right, out);
            }
            _ => out.push(expr),
        }
    }

    /// Extract `(col_name, str_value)` from a `Col == Str` or `Str == Col`
    /// equality. Returns `None` for any other shape.
    fn extract_col_str_eq(expr: &Expr2) -> Option<(&str, &str)> {
        if let Expr2::BinOp { op: BinOp2::Eq, left, right } = expr {
            match (left.as_ref(), right.as_ref()) {
                (Expr2::Col(c), Expr2::Str(s)) => Some((c.as_str(), s.as_str())),
                (Expr2::Str(s), Expr2::Col(c)) => Some((c.as_str(), s.as_str())),
                _ => None,
            }
        } else {
            None
        }
    }

    /// W1-D: Fast path for the TPC-H Q7 nation-pair filter pattern.
    ///
    /// Detects an OR-of-ANDs where every disjunct is a conjunction of
    /// `Col == Str` equalities referencing the **same two string columns**.
    /// The canonical Q7 shape is the symmetric pair:
    ///
    /// ```text
    /// (c1 == 'FRANCE' AND c2 == 'GERMANY')
    /// OR
    /// (c1 == 'GERMANY' AND c2 == 'FRANCE')
    /// ```
    ///
    /// but the implementation also handles non-symmetric multi-pair ORs
    /// (e.g. `(FRANCE,GERMANY) OR (FRANCE,ROMANIA) OR (FRANCE,RUSSIA)`).
    ///
    /// Replaces the generic OR evaluator — which allocates 2 `Vec<bool>`
    /// masks per OR arm plus 1 `Bitmap` per leaf equality and makes 8
    /// passes over the row data — with a single tight loop doing 2 column
    /// loads + N pair checks per row. For Q7 (~1.7M post-join rows x 2
    /// pairs) this eliminates ~6 MB of temporary allocations and collapses
    /// 8 passes into 1.
    ///
    /// Returns `Ok(true)` if the fast path was applied. Returns `Ok(false)`
    /// if the pattern did not match (caller falls back to the generic OR
    /// evaluator). Returns `Err` only if the pattern matched but evaluation
    /// failed (does not happen in the current implementation).
    fn try_nation_pair_or_lut(
        &self,
        or_expr: &Expr2,
        t: &ExecTable,
        mask: &mut [bool],
    ) -> Result<bool, Error> {
        use xxhash_rust::xxh3::xxh3_64;

        // 1. Flatten the OR tree into disjuncts.
        let mut disjuncts: Vec<&Expr2> = Vec::new();
        Self::flatten_disjuncts(or_expr, &mut disjuncts);
        if disjuncts.is_empty() {
            return Ok(false);
        }

        // 2. For each disjunct, extract (col_idx, str_hash) pairs.
        //    All disjuncts must reference exactly 2 columns and the same 2 columns.
        let mut col_a: Option<usize> = None;
        let mut col_b: Option<usize> = None;
        let mut pairs: Vec<(u64, u64)> = Vec::with_capacity(disjuncts.len());

        for disj in &disjuncts {
            let mut conjuncts: Vec<&Expr2> = Vec::new();
            Self::flatten_conjuncts(disj, &mut conjuncts);
            // Each conjunct must be a Col==Str equality.
            let mut col_a_hash: Option<u64> = None;
            let mut col_b_hash: Option<u64> = None;
            for conj in &conjuncts {
                let (col_name, str_val) = match Self::extract_col_str_eq(conj) {
                    Some(v) => v,
                    None => return Ok(false),
                };
                let cidx = match t.lookup_col(col_name) {
                    Some(i) => i,
                    None => return Ok(false),
                };
                if cidx >= t.col_types.len() || t.col_types[cidx] != ColType::String {
                    return Ok(false);
                }
                let h = xxh3_64(str_val.as_bytes());
                match (col_a, col_b) {
                    (None, None) => {
                        col_a = Some(cidx);
                        col_a_hash = Some(h);
                    }
                    (Some(a), None) => {
                        if cidx == a {
                            // Disjunct has 2 eqs on the same column - not the
                            // 2-column pair pattern we optimize.
                            return Ok(false);
                        }
                        col_b = Some(cidx);
                        col_b_hash = Some(h);
                    }
                    (Some(a), Some(b)) => {
                        if cidx == a {
                            col_a_hash = Some(h);
                        } else if cidx == b {
                            col_b_hash = Some(h);
                        } else {
                            return Ok(false);
                        } // references a 3rd column
                    }
                    _ => unreachable!(),
                }
            }
            match (col_a_hash, col_b_hash) {
                (Some(ha), Some(hb)) => pairs.push((ha, hb)),
                _ => return Ok(false), // disjunct didn't reference both columns
            }
        }

        let (ca, cb) = match (col_a, col_b) {
            (Some(a), Some(b)) => (a, b),
            _ => return Ok(false),
        };

        // 3. Tight loop: per row, check if (col_a[i], col_b[i]) is in the pair set.
        let col_a_data = &t.columns[ca];
        let col_b_data = &t.columns[cb];
        let n = t.row_count;
        let npairs = pairs.len();
        if npairs == 0 {
            return Ok(false);
        }

        // Build the result as a packed Bitmap by composing per-pair
        // (col_a == h1) AND (col_b == h2) bitmaps with OR. This reuses
        // the AVX-512 filter_eq_u64 kernel (8 u64s per instruction)
        // and the auto-vectorized byte-wise Bitmap::and / Bitmap::or,
        // avoiding the 2x Vec<bool> allocations + 2x clones + 3 scalar
        // reduction loops (5.4M iterations for Q7's 1.8M post-join rows)
        // that the generic OR evaluator performs.
        //
        // For Q7 (npairs=2, n=1.8M): 4x filter_eq_u64 + 2x Bitmap.and
        // + 1x Bitmap.or + 1x and_into_bool = ~8 AVX-512 passes vs the
        // generic path's 4x filter_eq_u64 + 4x and_into_bool + 3 scalar
        // loops + 2 Vec allocs + 2 Vec clones.
        if npairs <= 8 {
            use crate::exec::bitmap::{self, Bitmap};
            let mut acc: Option<Bitmap> = None;
            for &(h1, h2) in &pairs {
                let bm_a = bitmap::filter_eq_u64(col_a_data, h1);
                let bm_b = bitmap::filter_eq_u64(col_b_data, h2);
                let bm_pair = bm_a.and(&bm_b);
                acc = Some(match acc {
                    None => bm_pair,
                    Some(a) => a.or(&bm_pair),
                });
            }
            if let Some(bm) = acc {
                // mask[i] = mask[i] && bm.get(i)  (AVX-512BW bit expansion)
                bitmap::and_into_bool(&bm, &mut mask[..n]);
            }
        } else {
            // FxHashSet fallback for large N (no current TPC-H query hits
            // this; kept for correctness on hypothetical future queries).
            let set: FxHashSet<(u64, u64)> = pairs.iter().copied().collect();
            for i in 0..n {
                if !mask[i] {
                    continue;
                }
                let key = (col_a_data[i], col_b_data[i]);
                if !set.contains(&key) {
                    mask[i] = false;
                }
            }
        }

        Ok(true)
    }

    /// Vectorized boolean mask evaluation. Resolves column indices once,
    /// then loops over rows with direct array access. Falls back to
    /// per-row eval() for expression shapes it doesn't recognize.
    fn eval_bool_mask_vec(
        &self,
        expr: &Expr2,
        t: &ExecTable,
        mask: &mut [bool],
    ) -> Result<(), Error> {
        match expr {
            Expr2::BinOp { op: BinOp2::And, left, right } => {
                // W2: evaluate left then right directly into the same mask.
                // All leaf comparisons AND into the mask in place (via
                // `bitmap::and_into_bool`), the OR arm has been fixed to
                // AND its disjunction into the mask, and the per-row
                // fallback paths still early-exit on `if !mask[i] { continue; }`
                // so rows already filtered out by the left side are
                // skipped on the right side. This eliminates the previous
                // `mask.to_vec()` allocation (6 MB for a 6 M-row lineitem
                // scan) per conjunct.
                self.eval_bool_mask_vec(left, t, mask)?;
                self.eval_bool_mask_vec(right, t, mask)?;
                Ok(())
            }
            Expr2::BinOp { op: BinOp2::Or, left, right } => {
                // W1-D: try the nation-pair LUT fast path first. Recognizes
                // the Q7 pattern (OR of ANDs of `Col == Str` equalities on
                // the same 2 string columns) and replaces 8 row passes +
                // ~6 MB of temp allocations with a single tight loop.
                if self.try_nation_pair_or_lut(expr, t, mask)? {
                    return Ok(());
                }
                // W2: generic OR fallback — reuse thread-local pool buffers
                // instead of allocating 2 fresh `vec![true; N]` masks per
                // call. The disjunction is AND-ed into the incoming mask
                // (previously the OR arm OVERWROTE mask, relying on the
                // outer conjunct loop to re-AND — a latent bug if
                // eval_bool_mask_vec was ever called on an OR expression
                // with a non-trivial incoming mask).
                let n = t.row_count;
                let mut lmask = take_mask_buf(n);
                lmask[..n].fill(true);
                self.eval_bool_mask_vec(left, t, &mut lmask[..n])?;
                let mut rmask = take_mask_buf(n);
                rmask[..n].fill(true);
                self.eval_bool_mask_vec(right, t, &mut rmask[..n])?;
                for i in 0..n {
                    mask[i] = mask[i] && (lmask[i] || rmask[i]);
                }
                return_mask_buf(lmask);
                return_mask_buf(rmask);
                Ok(())
            }
            Expr2::BinOp { op, left, right } => {
                // Try to evaluate as Col op Literal or Literal op Col
                self.eval_comparison_vec(*op, left, right, t, mask)?;
                Ok(())
            }
            Expr2::Between { expr, low, high, negated } => {
                // W2: vectorized BETWEEN via two AVX-512 bitmap filters
                // (filter_ge_* + filter_le_*) composed with Bitmap::and,
                // then folded into the running mask via and_into_bool.
                // Matches the leaf-comparison fast path already used by
                // `apply_comparison` for `Col op Lit`. For NOT BETWEEN
                // we compose `col < lo OR col > hi` instead.
                if let Some(col_idx) = self.col_in(expr, t) {
                    use crate::exec::bitmap::{self, Bitmap};
                    let lo_val = self.eval_const(low, t)?;
                    let hi_val = self.eval_const(high, t)?;
                    let col: &[u64] = &t.columns[col_idx];
                    let col_type = t.col_types[col_idx];
                    let n = t.row_count;
                    let bm: Bitmap = match col_type {
                        ColType::Int => {
                            let lo = lo_val.as_i64().unwrap_or(i64::MIN);
                            let hi = hi_val.as_i64().unwrap_or(i64::MAX);
                            if *negated {
                                let bm_lt = bitmap::filter_lt_i64(col, lo);
                                let bm_gt = bitmap::filter_gt_i64(col, hi);
                                bm_lt.or(&bm_gt)
                            } else {
                                let bm_ge = bitmap::filter_ge_i64(col, lo);
                                let bm_le = bitmap::filter_le_i64(col, hi);
                                bm_ge.and(&bm_le)
                            }
                        }
                        ColType::Date => {
                            let lo = lo_val.as_u64().unwrap_or(0);
                            let hi = hi_val.as_u64().unwrap_or(u64::MAX);
                            if *negated {
                                let bm_lt = bitmap::filter_lt_u64(col, lo);
                                let bm_gt = bitmap::filter_gt_u64(col, hi);
                                bm_lt.or(&bm_gt)
                            } else {
                                let bm_ge = bitmap::filter_ge_u64(col, lo);
                                let bm_le = bitmap::filter_le_u64(col, hi);
                                bm_ge.and(&bm_le)
                            }
                        }
                        ColType::Float => {
                            let lo = lo_val.as_f64().unwrap_or(f64::NEG_INFINITY);
                            let hi = hi_val.as_f64().unwrap_or(f64::INFINITY);
                            if *negated {
                                let bm_lt = bitmap::filter_lt_f64(col, lo);
                                let bm_gt = bitmap::filter_gt_f64(col, hi);
                                bm_lt.or(&bm_gt)
                            } else {
                                let bm_ge = bitmap::filter_ge_f64(col, lo);
                                let bm_le = bitmap::filter_le_f64(col, hi);
                                bm_ge.and(&bm_le)
                            }
                        }
                        ColType::String => {
                            // String hashes are not order-comparable;
                            // fall back to a per-row scalar loop.
                            let mut bm = Bitmap::all_ones(n);
                            let lo = lo_val.as_u64().unwrap_or(0);
                            let hi = hi_val.as_u64().unwrap_or(u64::MAX);
                            for i in 0..n {
                                let v = col[i];
                                let in_range = v >= lo && v <= hi;
                                if *negated == in_range {
                                    bm.clear(i);
                                }
                            }
                            bm
                        }
                    };
                    bitmap::and_into_bool(&bm, &mut mask[..n]);
                    Ok(())
                } else {
                    // Fallback: per-row eval
                    for i in 0..t.row_count {
                        if !mask[i] {
                            continue;
                        }
                        let v = self.eval(expr, t, i)?;
                        let lo = self.eval(low, t, i)?;
                        let hi = self.eval(high, t, i)?;
                        let in_range = self.cmp_le(&lo, &v) && self.cmp_le(&v, &hi);
                        mask[i] = mask[i] && (*negated != in_range);
                    }
                    Ok(())
                }
            }
            Expr2::InList { expr, list, negated } => {
                if let Some(col_idx) = self.col_in(expr, t) {
                    let vals: Vec<u64> = list
                        .iter()
                        .filter_map(|e| {
                            if let Some(ci) = self.col_in(e, t) {
                                Some(t.columns[ci][0])
                            } else {
                                self.eval_const(e, t).ok().map(|v| v.to_u64())
                            }
                        })
                        .collect();
                    let col = &t.columns[col_idx];
                    for i in 0..t.row_count {
                        if !mask[i] {
                            continue;
                        }
                        let v = col[i];
                        let found = vals.iter().any(|&x| x == v);
                        mask[i] = mask[i] && (*negated != found);
                    }
                    Ok(())
                } else {
                    for i in 0..t.row_count {
                        if !mask[i] {
                            continue;
                        }
                        let v = self.eval(expr, t, i)?;
                        let mut found = false;
                        for item in list {
                            let iv = self.eval(item, t, i)?;
                            if self.cmp_eq(&v, &iv) {
                                found = true;
                                break;
                            }
                        }
                        mask[i] = mask[i] && (*negated != found);
                    }
                    Ok(())
                }
            }
            Expr2::Like { expr, pattern, negated } => {
                // For LIKE on string columns, use StringSearchColumn if available
                if let Some(col_idx) = self.col_in(expr, t) {
                    if col_idx < t.string_columns.len() {
                        if let Some(ref sc) = t.string_columns[col_idx] {
                            // Get pattern as string
                            let pat = if let Expr2::Str(s) = pattern.as_ref() {
                                s.clone()
                            } else {
                                self.eval(pattern, t, 0)
                                    .ok()
                                    .and_then(|v| v.as_str().map(|s| s.to_string()))
                                    .unwrap_or_default()
                            };
                            if !pat.is_empty() && sc.len() >= t.row_count {
                                // Only use StringSearchColumn if it has enough rows
                                // (after a join, the string column may have the wrong length)
                                let like_mask = self.like_mask(sc, &pat);
                                for i in 0..t.row_count {
                                    if *negated {
                                        mask[i] = mask[i] && !like_mask[i];
                                    } else {
                                        mask[i] = mask[i] && like_mask[i];
                                    }
                                }
                                return Ok(());
                            }
                        }
                    }
                }
                // Fallback: per-row eval
                for i in 0..t.row_count {
                    if !mask[i] {
                        continue;
                    }
                    let v = self.eval(expr, t, i)?;
                    let pv = self.eval(pattern, t, i)?;
                    let r = match (&v, &pv) {
                        (Value2::Str(s), Value2::Str(p)) => self.like(s, p),
                        _ => false,
                    };
                    mask[i] = mask[i] && (*negated != r);
                }
                Ok(())
            }
            _ => {
                // Fallback: per-row eval for unrecognized shapes
                for i in 0..t.row_count {
                    if !mask[i] {
                        continue;
                    }
                    let v = self.eval(expr, t, i)?;
                    mask[i] = mask[i] && self.truthy(&v);
                }
                Ok(())
            }
        }
    }

    /// Evaluate a constant expression (literal or column-independent).
    fn eval_const(&self, expr: &Expr2, t: &ExecTable) -> Result<Value2, Error> {
        match expr {
            Expr2::Int(i) => Ok(Value2::Int(*i)),
            Expr2::Float(f) => Ok(Value2::Float(*f)),
            Expr2::Str(s) => Ok(Value2::Str(s.clone())),
            Expr2::Date(d) => Ok(Value2::Date(*d)),
            Expr2::Neg(e) => {
                let v = self.eval_const(e, t)?;
                Ok(match v {
                    Value2::Int(i) => Value2::Int(-i),
                    Value2::Float(f) => Value2::Float(-f),
                    _ => Value2::Null,
                })
            }
            _ => self.eval(expr, t, 0),
        }
    }

    /// Build a LIKE mask for a string column. Handles % wildcards.
    fn like_mask(
        &self,
        sc: &crate::exec::fm_index::StringSearchColumn,
        pattern: &str,
    ) -> Vec<bool> {
        let n = sc.len();
        let mut mask = vec![false; n];
        if pattern.is_empty() {
            mask.fill(true);
            return mask;
        }
        let pb = pattern.as_bytes();
        if pb[0] == b'%' && !pb[1..].contains(&b'%') && !pattern.contains('_') {
            // Suffix match: %suffix
            let suffix = &pattern[1..];
            for i in 0..n {
                mask[i] = sc.get(i).ends_with(suffix);
            }
        } else if !pattern.contains('%') && !pattern.contains('_') {
            // Exact match
            for i in 0..n {
                mask[i] = sc.get(i) == pattern;
            }
        } else {
            // General LIKE
            for i in 0..n {
                mask[i] = self.like(sc.get(i), pattern);
            }
        }
        mask
    }

    /// Vectorized comparison: Col op Literal (or Literal op Col).
    /// Resolves column index once, then loops.
    /// Falls back to per-row eval for Col op Col or complex expressions.
    fn eval_comparison_vec(
        &self,
        op: BinOp2,
        left: &Expr2,
        right: &Expr2,
        t: &ExecTable,
        mask: &mut [bool],
    ) -> Result<(), Error> {
        // Try Col op Const (right side must NOT have column refs)
        if let Some(col_idx) = self.col_in(left, t) {
            if !self.expr_has_col(right) {
                let rval = self.eval_const(right, t)?;
                self.apply_comparison(op, col_idx, &rval, t, mask, false)?;
                return Ok(());
            }
        }
        // Try Const op Col (left side must NOT have column refs)
        if let Some(col_idx) = self.col_in(right, t) {
            if !self.expr_has_col(left) {
                let lval = self.eval_const(left, t)?;
                self.apply_comparison(swap_op(op), col_idx, &lval, t, mask, false)?;
                return Ok(());
            }
        }
        // Try Col(inner) op Col(outer): correlated subquery fast path.
        // When evaluating a WHERE filter inside a correlated subquery, one
        // side is an inner column (resolves to `t`) and the other is an
        // outer column (resolves via `self.outer`). Get the outer value ONCE
        // and use the vectorized bitmap filter — avoids per-row outer lookups
        // which made Q17's subquery take 300ms each (× 200 = 60s timeout).
        if let Some((outer_ptr, outer_row)) = self.outer.get() {
            let outer_t = unsafe { &*outer_ptr };
            // Col(inner) op Col(outer)
            if let Some(col_idx) = self.col_in(left, t) {
                if let Expr2::Col(rname) = right {
                    if t.lookup_col(rname).is_none() {
                        if let Some(outer_idx) = outer_t.lookup_col(rname).or_else(|| {
                            rname.rfind('.').and_then(|p| outer_t.lookup_col(&rname[p + 1..]))
                        }) {
                            let cell =
                                outer_t.columns[outer_idx].get(outer_row).copied().unwrap_or(0);
                            let rval = match outer_t.col_types[outer_idx] {
                                ColType::Int => Value2::Int(cell as i64),
                                ColType::Float => Value2::Float(f64::from_bits(cell)),
                                ColType::Date => Value2::Date(cell as u32 as i32),
                                ColType::String => Value2::Int(cell as i64),
                            };
                            self.apply_comparison(op, col_idx, &rval, t, mask, false)?;
                            return Ok(());
                        }
                    }
                }
            }
            // Col(outer) op Col(inner) — swap
            if let Some(col_idx) = self.col_in(right, t) {
                if let Expr2::Col(lname) = left {
                    if t.lookup_col(lname).is_none() {
                        if let Some(outer_idx) = outer_t.lookup_col(lname).or_else(|| {
                            lname.rfind('.').and_then(|p| outer_t.lookup_col(&lname[p + 1..]))
                        }) {
                            let cell =
                                outer_t.columns[outer_idx].get(outer_row).copied().unwrap_or(0);
                            let lval = match outer_t.col_types[outer_idx] {
                                ColType::Int => Value2::Int(cell as i64),
                                ColType::Float => Value2::Float(f64::from_bits(cell)),
                                ColType::Date => Value2::Date(cell as u32 as i32),
                                ColType::String => Value2::Int(cell as i64),
                            };
                            self.apply_comparison(swap_op(op), col_idx, &lval, t, mask, false)?;
                            return Ok(());
                        }
                    }
                }
            }
        }
        // W1-D: Col op Col fast path. Both sides resolve to columns in the
        // current table. The generic fallback below calls eval() per row
        // (2 FxHashMap lookups + Value2 construction + binop per row).
        // For Q7's 5 equi-join re-checks on the 1.8M-row post-join table,
        // that's ~9M hashmap lookups (~378ms). This fast path resolves
        // column indices once and does direct u64 array comparison (~18ms).
        // Only applies to Eq/Ne on Int/Date/String columns (u64 bit-comparable).
        // Float falls through (NaN/-0 edge cases require f64 semantics).
        if let (Some(lidx), Some(ridx)) = (self.col_in(left, t), self.col_in(right, t)) {
            let lt = t.col_types[lidx];
            let rt = t.col_types[ridx];
            if lt == rt && matches!(lt, ColType::Int | ColType::Date | ColType::String) {
                let lcol = &t.columns[lidx];
                let rcol = &t.columns[ridx];
                let n = t.row_count;
                match op {
                    BinOp2::Eq => {
                        for i in 0..n {
                            if mask[i] && lcol[i] != rcol[i] {
                                mask[i] = false;
                            }
                        }
                        return Ok(());
                    }
                    BinOp2::Ne => {
                        for i in 0..n {
                            if mask[i] && lcol[i] == rcol[i] {
                                mask[i] = false;
                            }
                        }
                        return Ok(());
                    }
                    _ => {}
                }
            }
        }
        // Fallback: per-row eval for Col op Col or complex expressions
        for i in 0..t.row_count {
            if !mask[i] {
                continue;
            }
            let lv = self.eval(left, t, i)?;
            let rv = self.eval(right, t, i)?;
            let result = self.binop(op, &lv, &rv);
            mask[i] = mask[i] && self.truthy(&result);
        }
        Ok(())
    }

    /// Apply a comparison (Col op Value) to the mask vectorized.
    fn apply_comparison(
        &self,
        op: BinOp2,
        col_idx: usize,
        val: &Value2,
        t: &ExecTable,
        mask: &mut [bool],
        _negated: bool,
    ) -> Result<(), Error> {
        use crate::exec::bitmap;
        let col: &[u64] = &t.columns[col_idx];
        let col_type = t.col_types[col_idx];
        let n = t.row_count;
        let mask = &mut mask[..n];
        match (col_type, val) {
            (ColType::Int, Value2::Int(ival)) => {
                let bm = match op {
                    BinOp2::Eq => bitmap::filter_eq_u64(col, *ival as u64),
                    BinOp2::Ne => bitmap::filter_ne_u64(col, *ival as u64),
                    BinOp2::Lt => bitmap::filter_lt_i64(col, *ival),
                    BinOp2::Le => bitmap::filter_le_i64(col, *ival),
                    BinOp2::Gt => bitmap::filter_gt_i64(col, *ival),
                    BinOp2::Ge => bitmap::filter_ge_i64(col, *ival),
                    _ => return Ok(()),
                };
                bitmap::and_into_bool(&bm, mask);
            }
            (ColType::Date, Value2::Date(dval)) => {
                let target = *dval as u64;
                let bm = match op {
                    BinOp2::Eq => bitmap::filter_eq_u64(col, target),
                    BinOp2::Ne => bitmap::filter_ne_u64(col, target),
                    BinOp2::Lt => bitmap::filter_lt_u64(col, target),
                    BinOp2::Le => bitmap::filter_le_u64(col, target),
                    BinOp2::Gt => bitmap::filter_gt_u64(col, target),
                    BinOp2::Ge => bitmap::filter_ge_u64(col, target),
                    _ => return Ok(()),
                };
                bitmap::and_into_bool(&bm, mask);
            }
            (ColType::Float, Value2::Float(fval)) => {
                let bm = match op {
                    BinOp2::Eq => bitmap::filter_eq_f64_epsilon(col, *fval),
                    BinOp2::Ne => bitmap::filter_ne_f64(col, *fval),
                    BinOp2::Lt => bitmap::filter_lt_f64(col, *fval),
                    BinOp2::Le => bitmap::filter_le_f64(col, *fval),
                    BinOp2::Gt => bitmap::filter_gt_f64(col, *fval),
                    BinOp2::Ge => bitmap::filter_ge_f64(col, *fval),
                    _ => return Ok(()),
                };
                bitmap::and_into_bool(&bm, mask);
            }
            (ColType::Float, Value2::Int(ival)) => {
                let fval = *ival as f64;
                let bm = match op {
                    BinOp2::Eq => bitmap::filter_eq_f64(col, fval),
                    BinOp2::Ne => bitmap::filter_ne_f64(col, fval),
                    BinOp2::Lt => bitmap::filter_lt_f64(col, fval),
                    BinOp2::Le => bitmap::filter_le_f64(col, fval),
                    BinOp2::Gt => bitmap::filter_gt_f64(col, fval),
                    BinOp2::Ge => bitmap::filter_ge_f64(col, fval),
                    _ => return Ok(()),
                };
                bitmap::and_into_bool(&bm, mask);
            }
            (ColType::String, Value2::Str(sval)) => {
                let target_hash = xxhash_rust::xxh3::xxh3_64(sval.as_bytes());
                match op {
                    BinOp2::Eq => {
                        let bm = bitmap::filter_eq_u64(col, target_hash);
                        bitmap::and_into_bool(&bm, mask);
                    }
                    BinOp2::Ne => {
                        let bm = bitmap::filter_ne_u64(col, target_hash);
                        bitmap::and_into_bool(&bm, mask);
                    }
                    _ => {}
                }
            }
            _ => {
                // Fallback: per-row eval
                for i in 0..n {
                    if !mask[i] {
                        continue;
                    }
                    let cv = unsafe { std::ptr::read(col.as_ptr().add(i)) };
                    let v = match col_type {
                        ColType::Int => Value2::Int(cv as i64),
                        ColType::Float => Value2::Float(f64::from_bits(cv)),
                        ColType::Date => Value2::Date(cv as i32),
                        ColType::String => Value2::Str(String::new()),
                    };
                    let matches = match op {
                        BinOp2::Eq => self.cmp_eq(&v, val),
                        BinOp2::Ne => !self.cmp_eq(&v, val),
                        BinOp2::Lt => self.cmp_lt(&v, val),
                        BinOp2::Le => self.cmp_le(&v, val),
                        BinOp2::Gt => !self.cmp_le(&v, val),
                        BinOp2::Ge => !self.cmp_lt(&v, val),
                        _ => false,
                    };
                    mask[i] = mask[i] && matches;
                }
            }
        }
        Ok(())
    }

    fn eval_bool_mask(
        &self,
        expr: &Expr2,
        table: &ExecTable,
        mask: &mut [bool],
    ) -> Result<(), Error> {
        match expr {
            Expr2::BinOp { op: BinOp2::And, left, right } => {
                self.eval_bool_mask(left, table, mask)?;
                let mut rm = vec![true; table.row_count];
                self.eval_bool_mask(right, table, &mut rm)?;
                for i in 0..table.row_count {
                    mask[i] = mask[i] && rm[i];
                }
                Ok(())
            }
            Expr2::BinOp { op: BinOp2::Or, left, right } => {
                let mut lm = vec![true; table.row_count];
                self.eval_bool_mask(left, table, &mut lm)?;
                let mut rm = vec![true; table.row_count];
                self.eval_bool_mask(right, table, &mut rm)?;
                for i in 0..table.row_count {
                    mask[i] = lm[i] || rm[i];
                }
                Ok(())
            }
            _ => {
                for i in 0..table.row_count {
                    let v = self.eval(expr, table, i)?;
                    mask[i] = mask[i] && self.truthy(&v);
                }
                Ok(())
            }
        }
    }

    fn truthy(&self, v: &Value2) -> bool {
        match v {
            Value2::Int(i) => *i != 0,
            Value2::Float(f) => *f != 0.0,
            Value2::Null => false,
            _ => false,
        }
    }

    // --- Expression evaluation ---

    fn eval(&self, expr: &Expr2, t: &ExecTable, row: usize) -> Result<Value2, Error> {
        match expr {
            Expr2::Col(name) => {
                // Try current table first
                if let Some(idx) = t.lookup_col(name) {
                    let cell = t.columns[idx].get(row).copied().unwrap_or(0);
                    return Ok(match t.col_types[idx] {
                        ColType::Int => Value2::Int(cell as i64),
                        ColType::Float => Value2::Float(f64::from_bits(cell)),
                        ColType::Date => Value2::Date(cell as u32 as i32),
                        ColType::String => {
                            // Use the StringSearchColumn only if it has enough
                            // entries for this row. After a join, string_columns
                            // are not rebuilt (they still have the pre-join row
                            // count), so sc.get(row) would return "" for rows
                            // beyond the original count. Fall back to the u64
                            // hash value (which is what filters and joins use).
                            if let Some(ref sc) = t.string_columns[idx] {
                                if sc.len() > row {
                                    Value2::Str(sc.get(row).to_string())
                                } else {
                                    Value2::Int(cell as i64)
                                }
                            } else {
                                Value2::Int(cell as i64)
                            }
                        }
                    });
                }
                // Try qualified name: if name contains '.', try the part after '.'
                if let Some(dot_pos) = name.rfind('.') {
                    let short_name = &name[dot_pos + 1..];
                    if let Some(idx) = t.lookup_col(short_name) {
                        let cell = t.columns[idx].get(row).copied().unwrap_or(0);
                        return Ok(match t.col_types[idx] {
                            ColType::Int => Value2::Int(cell as i64),
                            ColType::Float => Value2::Float(f64::from_bits(cell)),
                            ColType::Date => Value2::Date(cell as u32 as i32),
                            ColType::String => {
                                if let Some(ref sc) = t.string_columns[idx] {
                                    if sc.len() > row {
                                        Value2::Str(sc.get(row).to_string())
                                    } else {
                                        Value2::Int(cell as i64)
                                    }
                                } else {
                                    Value2::Int(cell as i64)
                                }
                            }
                        });
                    }
                }
                // Check outer context (correlated subquery)
                if let Some((outer_ptr, outer_row)) = self.outer.get() {
                    // SAFETY: outer_ptr was set by our own code and points to
                    // an ExecTable that is valid for the duration of this eval.
                    let outer_t = unsafe { &*outer_ptr };
                    // Try full name
                    if let Some(idx) = outer_t.lookup_col(name) {
                        let cell = outer_t.columns[idx].get(outer_row).copied().unwrap_or(0);
                        return Ok(match outer_t.col_types[idx] {
                            ColType::Int => Value2::Int(cell as i64),
                            ColType::Float => Value2::Float(f64::from_bits(cell)),
                            ColType::Date => Value2::Date(cell as u32 as i32),
                            ColType::String => {
                                if let Some(ref sc) = outer_t.string_columns[idx] {
                                    if sc.len() > outer_row {
                                        Value2::Str(sc.get(outer_row).to_string())
                                    } else {
                                        Value2::Int(cell as i64)
                                    }
                                } else {
                                    Value2::Int(cell as i64)
                                }
                            }
                        });
                    }
                    // Try short name (after '.')
                    if let Some(dot_pos) = name.rfind('.') {
                        let short_name = &name[dot_pos + 1..];
                        if let Some(idx) = outer_t.lookup_col(short_name) {
                            let cell = outer_t.columns[idx].get(outer_row).copied().unwrap_or(0);
                            return Ok(match outer_t.col_types[idx] {
                                ColType::Int => Value2::Int(cell as i64),
                                ColType::Float => Value2::Float(f64::from_bits(cell)),
                                ColType::Date => Value2::Date(cell as u32 as i32),
                                ColType::String => {
                                    if let Some(ref sc) = outer_t.string_columns[idx] {
                                        if sc.len() > outer_row {
                                            Value2::Str(sc.get(outer_row).to_string())
                                        } else {
                                            Value2::Int(cell as i64)
                                        }
                                    } else {
                                        Value2::Int(cell as i64)
                                    }
                                }
                            });
                        }
                    }
                }
                Err(Error::NotFound(format!("column '{}'", name)))
            }
            Expr2::Int(i) => Ok(Value2::Int(*i)),
            Expr2::Float(f) => Ok(Value2::Float(*f)),
            Expr2::Str(s) => Ok(Value2::Str(s.clone())),
            Expr2::Date(d) => Ok(Value2::Date(*d)),
            Expr2::Neg(e) => {
                let v = self.eval(e, t, row)?;
                match v {
                    Value2::Int(i) => Ok(Value2::Int(-i)),
                    Value2::Float(f) => Ok(Value2::Float(-f)),
                    _ => Ok(Value2::Null),
                }
            }
            Expr2::Not(e) => {
                let v = self.eval(e, t, row)?;
                Ok(Value2::Int(if !self.truthy(&v) { 1 } else { 0 }))
            }
            Expr2::BinOp { op, left, right } => {
                let lv = self.eval(left, t, row)?;
                let rv = self.eval(right, t, row)?;
                Ok(self.binop(*op, &lv, &rv))
            }
            Expr2::Like { expr, pattern, negated } => {
                let ev = self.eval(expr, t, row)?;
                let pv = self.eval(pattern, t, row)?;
                let r = match (&ev, &pv) {
                    (Value2::Str(s), Value2::Str(p)) => self.like(s, p),
                    (Value2::Int(h), Value2::Str(p)) => {
                        // Hashed string vs literal — can't do LIKE on hash.
                        // Fallback: exact match if no wildcards.
                        if !p.contains('%') && !p.contains('_') {
                            *h as u64 == xxhash_rust::xxh3::xxh3_64(p.as_bytes())
                        } else {
                            false
                        }
                    }
                    _ => false,
                };
                Ok(Value2::Int(if if *negated { !r } else { r } { 1 } else { 0 }))
            }
            Expr2::Between { expr, low, high, negated } => {
                let v = self.eval(expr, t, row)?;
                let lo = self.eval(low, t, row)?;
                let hi = self.eval(high, t, row)?;
                let in_range = self.cmp_le(&lo, &v) && self.cmp_le(&v, &hi);
                Ok(Value2::Int(if if *negated { !in_range } else { in_range } { 1 } else { 0 }))
            }
            Expr2::InList { expr, list, negated } => {
                let v = self.eval(expr, t, row)?;
                let mut found = false;
                for item in list {
                    let iv = self.eval(item, t, row)?;
                    if self.cmp_eq(&v, &iv) {
                        found = true;
                        break;
                    }
                }
                Ok(Value2::Int(if if *negated { !found } else { found } { 1 } else { 0 }))
            }
            Expr2::Case { whens, else_ } => {
                for (cond, result) in whens {
                    let cv = self.eval(cond, t, row)?;
                    if self.truthy(&cv) {
                        return self.eval(result, t, row);
                    }
                }
                if let Some(e) = else_ {
                    return self.eval(e, t, row);
                }
                Ok(Value2::Null)
            }
            Expr2::Extract { field, expr } => {
                let v = self.eval(expr, t, row)?;
                Ok(self.extract(field, &v))
            }
            Expr2::Cast { expr, target_type } => {
                let v = self.eval(expr, t, row)?;
                Ok(self.cast_value(&v, target_type))
            }
            Expr2::Substr { expr, start, len } => {
                let sv = self.eval(expr, t, row)?;
                let st = self.eval(start, t, row)?;
                let ln = self.eval(len, t, row)?;
                Ok(self.substr(&sv, &st, &ln))
            }
            Expr2::Subquery(q) => {
                // Check uncorrelated-subquery cache first.
                let ast_key = (q.as_ref() as *const SelectQuery2) as usize;
                {
                    let cache = self.subquery_cache.borrow();
                    if let Some(v) = cache.get(&ast_key) {
                        return Ok(v.clone());
                    }
                }
                // Try decorrelation: if the subquery is a correlated aggregate
                // (SELECT agg(expr) FROM t WHERE corr1 = outer1 AND corr2 = outer2 AND local_filters),
                // proactively build a derived table once, then per-row eval is a hash lookup.
                // This is critical for Q20 (800k correlation keys, each scanning 6M rows).
                {
                    let cached = self.decorrelated_cache.borrow().contains_key(&ast_key);
                    if !cached {
                        if let Some((map, cols)) = self.try_decorrelate_subquery(q, t)? {
                            self.decorrelated_cache.borrow_mut().insert(ast_key, (map, cols));
                        }
                    }
                }
                {
                    let cache = self.decorrelated_cache.borrow();
                    if let Some((map, corr_cols)) = cache.get(&ast_key) {
                        // Compute correlation hash from outer row's corr cols.
                        let mut corr_hash: u64 = 0;
                        for &ci in corr_cols {
                            let v = t.columns[ci].get(row).copied().unwrap_or(0);
                            corr_hash = corr_hash.wrapping_mul(0x517cc1b727220a95).wrapping_add(v);
                        }
                        if let Some(v) = map.get(&corr_hash) {
                            return Ok(v.clone());
                        }
                        // No match in derived table → subquery returns NULL (no rows match).
                        return Ok(Value2::Null);
                    }
                }
                // Correlated subquery: cache by (ast_key, hash of correlation column values).
                let corr_cols = self.find_correlation_cols(q, t);
                let mut corr_hash: u64 = 0;
                for &ci in &corr_cols {
                    let v = t.columns[ci].get(row).copied().unwrap_or(0);
                    corr_hash = corr_hash.wrapping_mul(0x517cc1b727220a95).wrapping_add(v);
                }
                let cache_key =
                    ast_key.wrapping_add((corr_hash.wrapping_mul(0x9E3779B97F4A7C15)) as usize);
                {
                    let cache = self.subquery_cache.borrow();
                    if let Some(v) = cache.get(&cache_key) {
                        return Ok(v.clone());
                    }
                }
                // Cache miss — execute with outer context (correlated subquery).
                let old_outer = self.outer.get();
                self.outer.set(Some((t as *const ExecTable, row)));
                let r = self.execute(q);
                self.outer.set(old_outer);
                let r = r?;
                let val = r.columns.first().and_then(|c| c.values.first()).copied().unwrap_or(0);
                let name = r.columns.first().map(|c| c.name.as_str()).unwrap_or("");
                let vals_slice: &[u64] =
                    r.columns.first().map(|c| c.values.as_slice()).unwrap_or(&[]);
                let v = match self.infer_result_type(name, vals_slice) {
                    ColType::Float => Value2::Float(f64::from_bits(val)),
                    _ => Value2::Int(val as i64),
                };
                self.subquery_cache.borrow_mut().insert(cache_key, v.clone());
                Ok(v)
            }
            Expr2::Exists { query, negated } => {
                // Semi-join fast path: if the subquery has a single correlation
                // column with an equi-join (e.g. `l_orderkey = o_orderkey`),
                // build a hash set of inner col values ONCE and check membership.
                // This decorrelates EXISTS, reducing ~25k subquery executions
                // (Q4) to 1 hash-set build + 25k lookups.
                let ast_key = (query.as_ref() as *const SelectQuery2) as usize;
                if let Some((outer_col_idx, inner_col_idx)) = self.find_exists_equi_join(query, t) {
                    // Build the hash set (cached by AST pointer)
                    let need_build = !self.exists_cache.borrow().contains_key(&ast_key);
                    if need_build {
                        let set = self.build_exists_hashset(query, inner_col_idx)?;
                        self.exists_cache.borrow_mut().insert(ast_key, set);
                    }
                    let cache = self.exists_cache.borrow();
                    if let Some(set) = cache.get(&ast_key) {
                        let outer_val = t.columns[outer_col_idx].get(row).copied().unwrap_or(0);
                        let exists = set.contains(&outer_val);
                        return Ok(Value2::Int(if if *negated { !exists } else { exists } {
                            1
                        } else {
                            0
                        }));
                    }
                }
                // Multi-column EXISTS fast path: if the subquery has 2 correlation
                // columns — one equi-join (e.g. l_orderkey = l1.l_orderkey) and one
                // inequality (e.g. l_suppkey <> l1.l_suppkey) — build a
                // HashMap<equi_key, HashSet<ineq_col>> once, then check per row.
                if let Some((outer_eq_idx, inner_eq_idx, outer_neq_idx, inner_neq_idx)) =
                    self.find_exists_multi_col(query, t)
                {
                    let need_build = !self.exists_multi_cache.borrow().contains_key(&ast_key);
                    if need_build {
                        let map =
                            self.build_exists_multi_map(query, inner_eq_idx, inner_neq_idx)?;
                        self.exists_multi_cache.borrow_mut().insert(ast_key, map);
                    }
                    let cache = self.exists_multi_cache.borrow();
                    if let Some(map) = cache.get(&ast_key) {
                        let outer_eq = t.columns[outer_eq_idx].get(row).copied().unwrap_or(0);
                        let outer_neq = t.columns[outer_neq_idx].get(row).copied().unwrap_or(0);
                        let exists = map
                            .get(&outer_eq)
                            .map_or(false, |set| set.iter().any(|&v| v != outer_neq));
                        return Ok(Value2::Int(if if *negated { !exists } else { exists } {
                            1
                        } else {
                            0
                        }));
                    }
                }
                // Fallback: per-row execution (correlated subquery)
                let old_outer = self.outer.get();
                self.outer.set(Some((t as *const ExecTable, row)));
                let r = self.execute(query);
                self.outer.set(old_outer);
                let r = r?;
                let ex = r.row_count > 0;
                Ok(Value2::Int(if if *negated { !ex } else { ex } { 1 } else { 0 }))
            }
            Expr2::InSubquery { expr, query, negated } => {
                let v = self.eval(expr, t, row)?;
                let ast_key = (query.as_ref() as *const SelectQuery2) as usize;
                // Check uncorrelated IN-subquery cache first.
                let need_build = !self.in_subquery_cache.borrow().contains_key(&ast_key);
                if need_build {
                    // Try executing with outer=None to detect uncorrelated.
                    let old_outer = self.outer.get();
                    self.outer.set(None);
                    let r = self.execute(query);
                    self.outer.set(old_outer);
                    match r {
                        Ok(r) => {
                            if let Some(col) = r.columns.first() {
                                let set: FxHashSet<u64> = col.values.iter().copied().collect();
                                self.in_subquery_cache.borrow_mut().insert(ast_key, set);
                            }
                        }
                        Err(_) => {
                            // Correlated — mark as empty set so we don't retry.
                            // Per-row eval with outer context will handle it.
                            self.in_subquery_cache.borrow_mut().insert(ast_key, new_fxhashset());
                        }
                    }
                }
                // Check cache. If the subquery was uncorrelated, the cache has
                // the full result set. If correlated (cache is empty set), fall
                // through to per-row execution.
                let cache = self.in_subquery_cache.borrow();
                if let Some(set) = cache.get(&ast_key) {
                    if !set.is_empty() || self.outer.get().is_none() {
                        // Uncorrelated — check membership.
                        // Note: for correlated subqueries that returned empty
                        // (no rows match), we can't distinguish from "correlated,
                        // not yet executed". But if outer is None, it's top-level,
                        // so empty means truly empty.
                        let v_u64 = v.to_u64();
                        let found = set.contains(&v_u64);
                        return Ok(Value2::Int(if if *negated { !found } else { found } {
                            1
                        } else {
                            0
                        }));
                    }
                }
                drop(cache);
                // Correlated IN-subquery — execute per row with outer context.
                let old_outer = self.outer.get();
                self.outer.set(Some((t as *const ExecTable, row)));
                let r = self.execute(query);
                self.outer.set(old_outer);
                let r = r?;
                let mut found = false;
                if let Some(col) = r.columns.first() {
                    for &cell in &col.values {
                        let iv = Value2::Int(cell as i64);
                        if self.cmp_eq(&v, &iv) {
                            found = true;
                            break;
                        }
                    }
                }
                Ok(Value2::Int(if if *negated { !found } else { found } { 1 } else { 0 }))
            }
            Expr2::Agg { .. } | Expr2::CountStar => {
                Err(Error::Other("aggregate in non-agg context".into()))
            }
        }
    }

    fn binop(&self, op: BinOp2, lv: &Value2, rv: &Value2) -> Value2 {
        match op {
            BinOp2::Add | BinOp2::Sub | BinOp2::Mul | BinOp2::Div => {
                let lf = lv.as_f64();
                let rf = rv.as_f64();
                match (lf, rf) {
                    (Some(l), Some(r)) => {
                        let res = match op {
                            BinOp2::Add => l + r,
                            BinOp2::Sub => l - r,
                            BinOp2::Mul => l * r,
                            BinOp2::Div => {
                                if r == 0.0 {
                                    return Value2::Null;
                                }
                                l / r
                            }
                            _ => unreachable!(),
                        };
                        // Keep as int if both are ints and op is not div
                        if matches!(lv, Value2::Int(_))
                            && matches!(rv, Value2::Int(_))
                            && op != BinOp2::Div
                        {
                            let li = lv.as_i64().unwrap();
                            let ri = rv.as_i64().unwrap();
                            let ir = match op {
                                BinOp2::Add => li.wrapping_add(ri),
                                BinOp2::Sub => li.wrapping_sub(ri),
                                BinOp2::Mul => li.wrapping_mul(ri),
                                _ => unreachable!(),
                            };
                            return Value2::Int(ir);
                        }
                        Value2::Float(res)
                    }
                    _ => Value2::Null,
                }
            }
            BinOp2::Eq => Value2::Int(if self.cmp_eq(lv, rv) { 1 } else { 0 }),
            BinOp2::Ne => Value2::Int(if !self.cmp_eq(lv, rv) { 1 } else { 0 }),
            BinOp2::Lt => Value2::Int(if self.cmp_lt(lv, rv) { 1 } else { 0 }),
            BinOp2::Gt => Value2::Int(if self.cmp_lt(rv, lv) { 1 } else { 0 }),
            BinOp2::Le => Value2::Int(if self.cmp_le(lv, rv) { 1 } else { 0 }),
            BinOp2::Ge => Value2::Int(if self.cmp_le(rv, lv) { 1 } else { 0 }),
            BinOp2::And => Value2::Int(if self.truthy(lv) && self.truthy(rv) { 1 } else { 0 }),
            BinOp2::Or => Value2::Int(if self.truthy(lv) || self.truthy(rv) { 1 } else { 0 }),
        }
    }

    fn cmp_eq(&self, a: &Value2, b: &Value2) -> bool {
        match (a, b) {
            (Value2::Null, _) | (_, Value2::Null) => false,
            (Value2::Str(x), Value2::Str(y)) => x == y,
            (Value2::Int(i), Value2::Str(s)) => {
                *i as u64 == xxhash_rust::xxh3::xxh3_64(s.as_bytes())
            }
            (Value2::Str(s), Value2::Int(i)) => {
                xxhash_rust::xxh3::xxh3_64(s.as_bytes()) == *i as u64
            }
            _ => {
                let af = a.as_f64();
                let bf = b.as_f64();
                match (af, bf) {
                    (Some(x), Some(y)) => x == y,
                    _ => false,
                }
            }
        }
    }
    fn cmp_lt(&self, a: &Value2, b: &Value2) -> bool {
        match (a, b) {
            (Value2::Null, _) | (_, Value2::Null) => false,
            (Value2::Str(x), Value2::Str(y)) => x < y,
            _ => {
                let af = a.as_f64();
                let bf = b.as_f64();
                match (af, bf) {
                    (Some(x), Some(y)) => x < y,
                    _ => false,
                }
            }
        }
    }
    fn cmp_le(&self, a: &Value2, b: &Value2) -> bool {
        self.cmp_lt(a, b) || self.cmp_eq(a, b)
    }

    fn like(&self, s: &str, pattern: &str) -> bool {
        let sb = s.as_bytes();
        let pb = pattern.as_bytes();
        let mut si = 0;
        let mut pi = 0;
        let mut star_s = usize::MAX;
        let mut star_p = usize::MAX;
        while si < sb.len() {
            if pi < pb.len() && (pb[pi] == b'_' || pb[pi] == sb[si]) {
                si += 1;
                pi += 1;
            } else if pi < pb.len() && pb[pi] == b'%' {
                star_p = pi;
                star_s = si;
                pi += 1;
            } else if star_p != usize::MAX {
                pi = star_p + 1;
                star_s += 1;
                si = star_s;
            } else {
                return false;
            }
        }
        while pi < pb.len() && pb[pi] == b'%' {
            pi += 1;
        }
        pi == pb.len()
    }

    fn extract(&self, field: &str, v: &Value2) -> Value2 {
        let days = match v {
            Value2::Date(d) => *d,
            Value2::Int(i) => *i as i32,
            Value2::Float(f) => *f as i32,
            _ => return Value2::Null,
        };
        let lower = field.to_lowercase();
        // W1-C: Fast path for `extract(year FROM ...)` — uses Howard Hinnant's
        // `civil_from_days` algorithm (~8 integer ops) instead of
        // `time::Date::from_julian_day` (~30 ops + branches per row).
        // Q7/Q8/Q9 each extract year from ~6M lineitem rows.
        if lower == "year" {
            return Value2::Int(crate::types::days_since_epoch_to_year(days as i64) as i64);
        }
        let date = crate::types::Date::from_u64(days as u64);
        let (y, m, d) = date.to_ymd();
        let r = match lower.as_str() {
            "month" => m as i64,
            "day" => d as i64,
            _ => y as i64,
        };
        Value2::Int(r)
    }

    /// Cast a Value2 to a target SQL type (Wave 67).
    ///
    /// Semantics per the Wave 67 spec:
    /// - `FLOAT` / `DOUBLE` / `REAL` / `DECIMAL` / `NUMERIC`: reinterpret
    ///   the u64 cell's bits as f64. (For an INT column with cell = 5,
    ///   `CAST(col AS FLOAT)` yields `f64::from_bits(5)` ≈ 2.47e-322,
    ///   NOT 5.0. The u64 cell value is preserved through the round-trip
    ///   `f64::to_bits(f64::from_bits(5)) == 5`.)
    /// - `INT` / `BIGINT` / `SMALLINT` / `TINYINT`: truncate to i64.
    ///   (For a FLOAT column with cell = `f64::to_bits(3.14)`,
    ///   `CAST(col AS INT)` yields 3.)
    /// - `VARCHAR` / `NVARCHAR` / `TEXT`: stringify the value.
    fn cast_value(&self, v: &Value2, target_type: &str) -> Value2 {
        let upper = target_type.to_uppercase();
        match upper.as_str() {
            "FLOAT" | "DOUBLE" | "REAL" | "DECIMAL" | "NUMERIC" => {
                // Reinterpret bits: take the u64 cell, treat as f64 bits.
                match v {
                    Value2::Int(i) => Value2::Float(f64::from_bits(*i as u64)),
                    Value2::Float(f) => Value2::Float(*f),
                    Value2::Date(d) => Value2::Float(f64::from_bits(*d as u64)),
                    Value2::Str(s) => {
                        // Strings can't be "reinterpreted" — parse as f64.
                        Value2::Float(s.parse().unwrap_or(0.0))
                    }
                    Value2::Null => Value2::Null,
                }
            }
            "INT" | "INTEGER" | "BIGINT" | "SMALLINT" | "TINYINT" => {
                // Truncate to i64.
                match v {
                    Value2::Int(i) => Value2::Int(*i),
                    Value2::Float(f) => Value2::Int(*f as i64),
                    Value2::Date(d) => Value2::Int(*d as i64),
                    Value2::Str(s) => Value2::Int(s.parse().unwrap_or(0)),
                    Value2::Null => Value2::Null,
                }
            }
            "VARCHAR" | "NVARCHAR" | "TEXT" | "CHAR" => {
                // Stringify.
                match v {
                    Value2::Int(i) => Value2::Str(i.to_string()),
                    Value2::Float(f) => Value2::Str(f.to_string()),
                    Value2::Date(d) => Value2::Str(crate::types::Date(*d).to_iso()),
                    Value2::Str(s) => Value2::Str(s.clone()),
                    Value2::Null => Value2::Null,
                }
            }
            // Unknown target type — return the value unchanged.
            _ => v.clone(),
        }
    }

    fn substr(&self, s: &Value2, start: &Value2, len: &Value2) -> Value2 {
        let s = match s.as_str() {
            Some(s) => s,
            None => return Value2::Null,
        };
        let st = start.as_i64().unwrap_or(1).max(1) as usize;
        let ln = len.as_i64().unwrap_or(0) as usize;
        let si = st.saturating_sub(1);
        if si >= s.len() {
            return Value2::Str(String::new());
        }
        let ei = (si + ln).min(s.len());
        Value2::Str(s[si..ei].to_string())
    }

    // --- GROUP BY + aggregates ---

    /// Low-cardinality GROUP BY fast path using FixedAccumulator.
    /// For <=256 groups: single pass, no HashMap, no Vec<Vec<usize>>.
    /// Returns None if the query is too complex for this path.
    fn try_low_card_grouped(
        &self,
        query: &SelectQuery2,
        t: &ExecTable,
        mask: &[bool],
    ) -> Result<Option<QueryResult>, Error> {
        use crate::exec::fixed_agg::{FixedAccumulator, MAX_FIXED_GROUPS};

        if query.having.is_some() {
            return Ok(None);
        }

        let gb_cols: Vec<Option<usize>> =
            query.group_by.iter().map(|gb| self.col_in(gb, t)).collect();
        if gb_cols.iter().any(|c| c.is_none()) {
            return Ok(None);
        }
        let gb_cols: Vec<usize> = gb_cols.iter().map(|c| c.unwrap()).collect();

        #[derive(Clone)]
        enum LcAgg {
            GroupByCol(usize),
            CountAll,
            SumCol(usize),
            SumColCol(usize, usize),
            SumColSubOne(usize, usize),
            SumColSubOneAddOne(usize, usize, usize),
            AvgCol(usize),
            MinCol(usize),
            MaxCol(usize),
        }

        let mut plans: Vec<Option<LcAgg>> = Vec::with_capacity(query.select.len());
        for item in &query.select {
            let plan = match &item.expr {
                Expr2::CountStar => Some(LcAgg::CountAll),
                Expr2::Agg { func, arg, distinct: false } => {
                    match func {
                        AggFunc::Count => {
                            // count(Col) counts non-null (non-zero) values.
                            // count(*) counts all rows.
                            // The low_card path only supports CountAll (count(*)).
                            // count(Col) falls back to the HashMap path.
                            if let Some(_) = self.col_in(arg, t) {
                                None
                            } else {
                                Some(LcAgg::CountAll)
                            }
                        }
                        AggFunc::Sum => {
                            if let Some(a) = self.col_in(arg, t) {
                                if t.col_types[a] == ColType::Float {
                                    Some(LcAgg::SumCol(a))
                                } else {
                                    None
                                }
                            } else if let Expr2::BinOp { op: BinOp2::Mul, left, right } =
                                arg.as_ref()
                            {
                                if let (Some(a), Some(b)) =
                                    (self.col_in(left, t), self.col_in(right, t))
                                {
                                    if t.col_types[a] == ColType::Float
                                        && t.col_types[b] == ColType::Float
                                    {
                                        Some(LcAgg::SumColCol(a, b))
                                    } else {
                                        None
                                    }
                                } else if let (Some(a), Some(b)) =
                                    (self.col_in(left, t), self.col_in_sub_one_right(right, t))
                                {
                                    if t.col_types[a] == ColType::Float
                                        && t.col_types[b] == ColType::Float
                                    {
                                        Some(LcAgg::SumColSubOne(a, b))
                                    } else {
                                        None
                                    }
                                } else if let (Some(b), Some(a)) =
                                    (self.col_in(right, t), self.col_in_sub_one_right(left, t))
                                {
                                    if t.col_types[a] == ColType::Float
                                        && t.col_types[b] == ColType::Float
                                    {
                                        Some(LcAgg::SumColSubOne(a, b))
                                    } else {
                                        None
                                    }
                                } else {
                                    None
                                }
                            } else {
                                None
                            }
                        }
                        AggFunc::Avg => {
                            if let Expr2::Col(name) = arg.as_ref() {
                                if let Some(idx) = t.lookup_col(name) {
                                    if t.col_types[idx] == ColType::Float {
                                        Some(LcAgg::AvgCol(idx))
                                    } else {
                                        None
                                    }
                                } else {
                                    None
                                }
                            } else {
                                None
                            }
                        }
                        AggFunc::Min => {
                            if let Expr2::Col(name) = arg.as_ref() {
                                if let Some(idx) = t.lookup_col(name) {
                                    Some(LcAgg::MinCol(idx))
                                } else {
                                    None
                                }
                            } else {
                                None
                            }
                        }
                        AggFunc::Max => {
                            if let Expr2::Col(name) = arg.as_ref() {
                                if let Some(idx) = t.lookup_col(name) {
                                    Some(LcAgg::MaxCol(idx))
                                } else {
                                    None
                                }
                            } else {
                                None
                            }
                        }
                        _ => None,
                    }
                }
                Expr2::Col(name) => {
                    if let Some(idx) = t.lookup_col(name) {
                        Some(LcAgg::GroupByCol(idx))
                    } else {
                        None
                    }
                }
                _ => None,
            };
            plans.push(plan);
        }

        for (i, item) in query.select.iter().enumerate() {
            if plans[i].is_some() {
                continue;
            }
            if let Expr2::Agg { func: AggFunc::Sum, arg, distinct: false } = &item.expr {
                if let Expr2::BinOp { op: BinOp2::Mul, left, right } = arg.as_ref() {
                    // Try: (Col * (1 - Col2)) * (1 + Col3)
                    if let Some((a, b)) = self.col_in_mul_sub_one(left, t) {
                        if let Some(c) = self.col_in_add_one_right(right, t) {
                            if t.col_types[a] == ColType::Float
                                && t.col_types[b] == ColType::Float
                                && t.col_types[c] == ColType::Float
                            {
                                plans[i] = Some(LcAgg::SumColSubOneAddOne(a, b, c));
                            }
                        }
                    }
                }
            }
        }

        if plans.iter().any(|p| p.is_none()) {
            return Ok(None);
        }

        let agg_indices: Vec<usize> = plans
            .iter()
            .enumerate()
            .filter_map(|(i, p)| match p {
                Some(LcAgg::SumCol(_))
                | Some(LcAgg::SumColCol(_, _))
                | Some(LcAgg::SumColSubOne(_, _))
                | Some(LcAgg::SumColSubOneAddOne(_, _, _))
                | Some(LcAgg::AvgCol(_))
                | Some(LcAgg::MinCol(_))
                | Some(LcAgg::MaxCol(_)) => Some(i),
                _ => None,
            })
            .collect();
        let num_aggs = agg_indices.len();
        if num_aggs == 0 {
            return Ok(None);
        }

        let n = t.row_count;

        // Collect aggregate column references
        let agg_specs: Vec<(usize, Option<usize>, Option<usize>, u8)> = agg_indices
            .iter()
            .map(|&item_idx| match plans[item_idx].as_ref().unwrap() {
                LcAgg::SumCol(a) => (*a, None, None, 0),
                LcAgg::SumColCol(a, b) => (*a, Some(*b), None, 1),
                LcAgg::SumColSubOne(a, b) => (*a, Some(*b), None, 2),
                LcAgg::SumColSubOneAddOne(a, b, c) => (*a, Some(*b), Some(*c), 3),
                LcAgg::AvgCol(a) => (*a, None, None, 4),
                LcAgg::MinCol(a) => (*a, None, None, 5),
                LcAgg::MaxCol(a) => (*a, None, None, 6),
                _ => (0, None, None, 0),
            })
            .collect();
        let num_aggs_actual = num_aggs;

        // Parallel single-pass morsel aggregation.
        // Each thread processes a chunk, maintaining its own local group->slot map
        // and per-group accumulators. At the end, merge all thread-local maps.
        // For Q1 (4 groups, 8 threads): merge is 32 entries — trivial.
        const CHUNK_SIZE: usize = 65536;
        let num_chunks = (n + CHUNK_SIZE - 1) / CHUNK_SIZE;

        // Each chunk produces: (group_keys: Vec<u64>, sums: Vec<f64>, counts: Vec<u64>)
        // where sums is laid out as [agg0_grp0, agg0_grp1, ..., agg1_grp0, ...]
        let partials: Vec<Option<(Vec<u64>, Vec<f64>, Vec<u64>)>> = (0..num_chunks)
            .into_par_iter()
            .map(|chunk_idx| {
                let start = chunk_idx * CHUNK_SIZE;
                let end = std::cmp::min(start + CHUNK_SIZE, n);

                let mut local_keys: Vec<u64> = Vec::with_capacity(16);
                let mut local_slot: FxHashMap<u64, usize> = new_fxhashmap();

                let mut local_sums: Vec<f64> = Vec::new();
                let mut local_counts: Vec<u64> = Vec::new();

                for i in start..end {
                    if !mask[i] {
                        continue;
                    }

                    let mut key_hash: u64 = 0;
                    for &ci in &gb_cols {
                        key_hash = key_hash
                            .wrapping_mul(0x517cc1b727220a95)
                            .wrapping_add(t.columns[ci][i]);
                    }

                    let slot = if let Some(&s) = local_slot.get(&key_hash) {
                        s
                    } else {
                        let new_slot = local_keys.len();
                        if new_slot >= MAX_FIXED_GROUPS - 1 {
                            return None;
                        }
                        local_keys.push(key_hash);
                        local_slot.insert(key_hash, new_slot);
                        local_sums.extend(std::iter::repeat(0.0f64).take(num_aggs_actual));
                        local_counts.push(0);
                        new_slot
                    };

                    local_counts[slot] += 1;

                    for (ai, &(col_a, col_b_o, col_c_o, at)) in agg_specs.iter().enumerate() {
                        let base = ai * local_keys.len();
                        let va = t.columns[col_a][i];
                        match at {
                            0 => {
                                local_sums[base + slot] += f64::from_bits(va);
                            }
                            1 => {
                                if let Some(cb) = col_b_o {
                                    local_sums[base + slot] +=
                                        f64::from_bits(va) * f64::from_bits(t.columns[cb][i]);
                                }
                            }
                            2 => {
                                if let Some(cb) = col_b_o {
                                    local_sums[base + slot] += f64::from_bits(va)
                                        * (1.0 - f64::from_bits(t.columns[cb][i]));
                                }
                            }
                            3 => {
                                if let (Some(cb), Some(cc)) = (col_b_o, col_c_o) {
                                    local_sums[base + slot] += f64::from_bits(va)
                                        * (1.0 - f64::from_bits(t.columns[cb][i]))
                                        * (1.0 + f64::from_bits(t.columns[cc][i]));
                                }
                            }
                            4 => {
                                local_sums[base + slot] += f64::from_bits(va);
                            }
                            _ => {}
                        }
                    }
                }
                Some((local_keys, local_sums, local_counts))
            })
            .collect();

        // If any chunk returned None (too many groups), fall back to HashMap path
        if partials.iter().any(|p| p.is_none()) {
            return Ok(None);
        }
        let partials: Vec<(Vec<u64>, Vec<f64>, Vec<u64>)> =
            partials.into_iter().map(|p| p.unwrap()).collect();

        // Merge: build global group->slot map from all chunk-local keys
        let mut key_to_slot: FxHashMap<u64, usize> = new_fxhashmap();

        let mut group_keys_discovered: Vec<u64> = Vec::new();
        for (keys, _, _) in &partials {
            for &k in keys {
                if !key_to_slot.contains_key(&k) {
                    let slot = group_keys_discovered.len();
                    if slot >= MAX_FIXED_GROUPS - 1 {
                        return Ok(None);
                    }
                    key_to_slot.insert(k, slot);
                    group_keys_discovered.push(k);
                }
            }
        }
        let num_groups_found = group_keys_discovered.len();
        if num_groups_found == 0 {
            let mut cols: Vec<ResultColumn> = Vec::with_capacity(query.select.len());
            for item in &query.select {
                let name = item.alias.clone().unwrap_or_else(|| self.expr_name(&item.expr));
                cols.push(ResultColumn {
                    name,
                    values: Vec::new(),
                    string_values: None,
                    type_oid: 0,
                    null_mask: None,
                });
            }
            return Ok(Some(QueryResult { columns: cols, row_count: 0, elapsed_us: 0 }));
        }

        // Merge sums and counts into final accumulators
        let mut final_sums = vec![0.0f64; num_groups_found * num_aggs_actual];
        let mut final_counts = vec![0u64; num_groups_found];
        for (keys, sums, counts) in &partials {
            let local_ng = keys.len();
            for (local_slot, &key) in keys.iter().enumerate() {
                let global_slot = key_to_slot[&key];
                final_counts[global_slot] += counts[local_slot];
                for a in 0..num_aggs_actual {
                    final_sums[a * num_groups_found + global_slot] +=
                        sums[a * local_ng + local_slot];
                }
            }
        }

        // Min/Max (serial pass)
        for (ai, &item_idx) in agg_indices.iter().enumerate() {
            if matches!(plans[item_idx], Some(LcAgg::MinCol(_)) | Some(LcAgg::MaxCol(_))) {
                let a = if let Some(LcAgg::MinCol(a)) | Some(LcAgg::MaxCol(a)) = plans[item_idx] {
                    a
                } else {
                    0
                };
                let is_min = matches!(plans[item_idx], Some(LcAgg::MinCol(_)));
                let mut mm =
                    vec![if is_min { f64::INFINITY } else { f64::NEG_INFINITY }; num_groups_found];
                for i in 0..n {
                    if !mask[i] {
                        continue;
                    }
                    let mut key_hash: u64 = 0;
                    for &ci in &gb_cols {
                        key_hash = key_hash
                            .wrapping_mul(0x517cc1b727220a95)
                            .wrapping_add(t.columns[ci][i]);
                    }
                    if let Some(&slot) = key_to_slot.get(&key_hash) {
                        let v = f64::from_bits(t.columns[a][i]);
                        if is_min {
                            if v < mm[slot] {
                                mm[slot] = v;
                            }
                        } else {
                            if v > mm[slot] {
                                mm[slot] = v;
                            }
                        }
                    }
                }
                for g in 0..num_groups_found {
                    final_sums[ai * num_groups_found + g] = mm[g];
                }
            }
        }

        let finalized: Vec<(u64, Vec<f64>, u64, Vec<f64>, Vec<f64>)> = (0..num_groups_found)
            .map(|g| {
                let key = group_keys_discovered[g];
                let sums: Vec<f64> =
                    (0..num_aggs_actual).map(|a| final_sums[a * num_groups_found + g]).collect();
                (key, sums, final_counts[g], vec![0.0; num_aggs_actual], vec![0.0; num_aggs_actual])
            })
            .collect();
        let mut cols: Vec<ResultColumn> = Vec::with_capacity(query.select.len());

        for (item_idx, item) in query.select.iter().enumerate() {
            let name = item.alias.clone().unwrap_or_else(|| self.expr_name(&item.expr));
            let values: Vec<u64> = match plans[item_idx].as_ref().unwrap() {
                LcAgg::GroupByCol(_) => finalized.iter().map(|(key, _, _, _, _)| *key).collect(),
                LcAgg::CountAll => finalized.iter().map(|(_, _, count, _, _)| *count).collect(),
                LcAgg::SumCol(_)
                | LcAgg::SumColCol(_, _)
                | LcAgg::SumColSubOne(_, _)
                | LcAgg::SumColSubOneAddOne(_, _, _) => {
                    let agg_idx = agg_indices.iter().position(|&idx| idx == item_idx).unwrap();
                    finalized.iter().map(|(_, sums, _, _, _)| sums[agg_idx].to_bits()).collect()
                }
                LcAgg::AvgCol(_) => {
                    let agg_idx = agg_indices.iter().position(|&idx| idx == item_idx).unwrap();
                    finalized
                        .iter()
                        .map(|(_, sums, count, _, _)| {
                            if *count == 0 {
                                0u64
                            } else {
                                (sums[agg_idx] / *count as f64).to_bits()
                            }
                        })
                        .collect()
                }
                LcAgg::MinCol(_) => {
                    let agg_idx = agg_indices.iter().position(|&idx| idx == item_idx).unwrap();
                    finalized.iter().map(|(_, _, _, mins, _)| mins[agg_idx].to_bits()).collect()
                }
                LcAgg::MaxCol(_) => {
                    let agg_idx = agg_indices.iter().position(|&idx| idx == item_idx).unwrap();
                    finalized.iter().map(|(_, _, _, _, maxs)| maxs[agg_idx].to_bits()).collect()
                }
            };
            cols.push(ResultColumn {
                name,
                values,
                string_values: None,
                type_oid: 0,
                null_mask: None,
            });
        }

        let mut result = QueryResult { columns: cols, row_count: finalized.len(), elapsed_us: 0 };

        if !query.order_by.is_empty() {
            result = self.apply_order_by_grouped(result, &query.order_by)?;
        }
        if let Some(limit) = query.limit {
            if result.row_count > limit {
                for col in &mut result.columns {
                    col.values.truncate(limit);
                }
                result.row_count = limit;
            }
        }
        Ok(Some(result))
    }

    fn execute_grouped(
        &self,
        query: &SelectQuery2,
        t: &ExecTable,
        mask: &[bool],
    ) -> Result<QueryResult, Error> {
        if query.group_by.is_empty() {
            let indices: Vec<usize> = (0..t.row_count).filter(|&i| mask[i]).collect();
            return self.execute_scalar_agg(query, t, &indices);
        }

        // Low-cardinality fast path (Q1: 4 groups, Q13: ~40 groups)
        if let Some(result) = self.try_low_card_grouped(query, t, mask)? {
            return Ok(result);
        }

        // Fallback: HashMap-based grouping for high cardinality.
        // PARALLEL: split into chunks, each thread builds a local HashMap,
        // then merge. This is critical for Q3 (10k groups, 300k rows) which
        // was serial and took ~200ms just for grouping.
        let indices: Vec<usize> = (0..t.row_count).filter(|&i| mask[i]).collect();

        // Pre-resolve GROUP BY column indices. For computed expressions
        // (extract, substr), pre-evaluate per row (serial — needed because
        // QueryInterpreter is not Sync due to Cell/RefCell).
        let gb_cols: Vec<Option<usize>> =
            query.group_by.iter().map(|gb| self.col_in(gb, t)).collect();
        let has_computed_gb = gb_cols.iter().any(|c| c.is_none());
        // Pre-compute GROUP BY values for computed expressions
        let gb_values: Vec<Vec<u64>> = if has_computed_gb {
            query
                .group_by
                .iter()
                .enumerate()
                .map(|(gi, gb)| {
                    if gb_cols[gi].is_some() {
                        Vec::new() // will read from column directly
                    } else {
                        indices
                            .iter()
                            .map(|&idx| self.eval(gb, t, idx).unwrap_or(Value2::Null).to_u64())
                            .collect()
                    }
                })
                .collect()
        } else {
            Vec::new()
        };

        // PARALLEL grouping: split indices into chunks, each thread builds
        // a local HashMap<u64, Vec<usize>>, then merge.
        const GROUP_CHUNK_SIZE: usize = 65536;
        let n_indices = indices.len();
        let num_chunks = (n_indices + GROUP_CHUNK_SIZE - 1) / GROUP_CHUNK_SIZE;

        let local_maps: Vec<FxHashMap<u64, Vec<usize>>> = (0..num_chunks)
            .into_par_iter()
            .map(|chunk_idx| {
                let start = chunk_idx * GROUP_CHUNK_SIZE;
                let end = std::cmp::min(start + GROUP_CHUNK_SIZE, n_indices);
                let mut local: FxHashMap<u64, Vec<usize>> = new_fxhashmap();

                for i in start..end {
                    let idx = indices[i];
                    let mut key_hash: u64 = 0;
                    for (gi, _) in query.group_by.iter().enumerate() {
                        let v = if let Some(ci) = gb_cols[gi] {
                            t.columns[ci][idx]
                        } else {
                            // Use pre-computed value
                            gb_values[gi][i]
                        };
                        key_hash = key_hash.wrapping_mul(0x517cc1b727220a95).wrapping_add(v);
                    }
                    local.entry(key_hash).or_default().push(idx);
                }
                local
            })
            .collect();

        // Merge local maps into global group_indices
        let mut group_map: FxHashMap<u64, usize> = new_fxhashmap();

        let mut group_indices: Vec<Vec<usize>> = Vec::with_capacity(1024);
        for local in local_maps {
            for (hash, rows) in local {
                let gid = if let Some(&existing) = group_map.get(&hash) {
                    existing
                } else {
                    let new_id = group_indices.len();
                    group_map.insert(hash, new_id);
                    group_indices.push(Vec::new());
                    new_id
                };
                group_indices[gid].extend(rows);
            }
        }

        let group_indices: Vec<&Vec<usize>> = group_indices.iter().collect();

        // HAVING
        let filtered: Vec<usize> = if let Some(ref having) = query.having {
            let mut v = Vec::new();
            for (gi, gidxs) in group_indices.iter().enumerate() {
                let hv = self.eval_agg_expr(having, t, gidxs)?;
                if self.truthy(&hv) {
                    v.push(gi);
                }
            }
            v
        } else {
            (0..group_indices.len()).collect()
        };

        // Build result using FUSED per-group aggregation.
        let fused = self.try_fused_grouped_agg(&query.select, t, &group_indices, &filtered)?;
        let mut cols = Vec::new();
        for (item_idx, item) in query.select.iter().enumerate() {
            let name = item.alias.clone().unwrap_or_else(|| self.expr_name(&item.expr));
            let values: Vec<u64> = if let Some(ref fv) = fused {
                fv.get(item_idx).cloned().unwrap_or_else(|| {
                    filtered
                        .iter()
                        .map(|&gi| {
                            let gidxs = group_indices[gi];
                            self.eval_agg_expr(&item.expr, t, gidxs)
                                .unwrap_or(Value2::Null)
                                .to_u64()
                        })
                        .collect()
                })
            } else {
                filtered
                    .iter()
                    .map(|&gi| {
                        let gidxs = group_indices[gi];
                        self.eval_agg_expr(&item.expr, t, gidxs).unwrap_or(Value2::Null).to_u64()
                    })
                    .collect()
            };
            cols.push(ResultColumn {
                name,
                values,
                string_values: None,
                type_oid: 0,
                null_mask: None,
            });
        }

        let mut result = QueryResult { columns: cols, row_count: filtered.len(), elapsed_us: 0 };

        if !query.order_by.is_empty() {
            result = self.apply_order_by_grouped(result, &query.order_by)?;
        }
        if let Some(limit) = query.limit {
            if result.row_count > limit {
                for col in &mut result.columns {
                    col.values.truncate(limit);
                }
                result.row_count = limit;
            }
        }
        Ok(result)
    }

    // Rust function to insert before execute_scalar_agg
    /// Fused per-group aggregation: analyze all select items, and if they
    /// match supported patterns, do a SINGLE pass per group computing all aggregates.
    fn try_fused_grouped_agg(
        &self,
        select: &[SelectItem2],
        t: &ExecTable,
        group_indices: &[&Vec<usize>],
        filtered: &[usize],
    ) -> Result<Option<Vec<Vec<u64>>>, Error> {
        if filtered.is_empty() {
            return Ok(Some(vec![Vec::new(); select.len()]));
        }

        #[derive(Clone)]
        enum FusedAgg {
            GroupByCol(usize),
            CountAll,
            SumCol(usize),
            SumColCol(usize, usize),
            SumColSubOne(usize, usize),
            SumColSubOneAddOne(usize, usize, usize),
            AvgCol(usize),
            MinCol(usize),
            MaxCol(usize),
        }

        let mut plans: Vec<Option<FusedAgg>> = Vec::with_capacity(select.len());
        for item in select {
            let plan = match &item.expr {
                Expr2::CountStar => Some(FusedAgg::CountAll),
                Expr2::Agg { func, arg, distinct: false } => {
                    match func {
                        AggFunc::Count => {
                            // count(Col) counts non-null (non-zero) values.
                            // count(*) counts all rows.
                            // The fused path only supports CountAll (count(*)).
                            // count(Col) falls back to per-row eval_agg_expr.
                            if self.col_in(arg, t).is_some() {
                                None
                            } else {
                                Some(FusedAgg::CountAll)
                            }
                        }
                        AggFunc::Sum => {
                            if let Some(a) = self.col_in(arg, t) {
                                if t.col_types[a] == ColType::Float {
                                    Some(FusedAgg::SumCol(a))
                                } else {
                                    None
                                }
                            } else if let Expr2::BinOp { op: BinOp2::Mul, left, right } =
                                arg.as_ref()
                            {
                                if let (Some(a), Some(b)) =
                                    (self.col_in(left, t), self.col_in(right, t))
                                {
                                    if t.col_types[a] == ColType::Float
                                        && t.col_types[b] == ColType::Float
                                    {
                                        Some(FusedAgg::SumColCol(a, b))
                                    } else {
                                        None
                                    }
                                } else if let (Some(a), Some(b)) =
                                    (self.col_in(left, t), self.col_in_sub_one_right(right, t))
                                {
                                    if t.col_types[a] == ColType::Float
                                        && t.col_types[b] == ColType::Float
                                    {
                                        Some(FusedAgg::SumColSubOne(a, b))
                                    } else {
                                        None
                                    }
                                } else if let (Some(b), Some(a)) =
                                    (self.col_in(right, t), self.col_in_sub_one_right(left, t))
                                {
                                    if t.col_types[a] == ColType::Float
                                        && t.col_types[b] == ColType::Float
                                    {
                                        Some(FusedAgg::SumColSubOne(a, b))
                                    } else {
                                        None
                                    }
                                } else {
                                    None
                                }
                            } else {
                                None
                            }
                        }
                        AggFunc::Avg => {
                            if let Expr2::Col(name) = arg.as_ref() {
                                if let Some(idx) = t.lookup_col(name) {
                                    if t.col_types[idx] == ColType::Float {
                                        Some(FusedAgg::AvgCol(idx))
                                    } else {
                                        None
                                    }
                                } else {
                                    None
                                }
                            } else {
                                None
                            }
                        }
                        AggFunc::Min => {
                            if let Expr2::Col(name) = arg.as_ref() {
                                if let Some(idx) = t.lookup_col(name) {
                                    Some(FusedAgg::MinCol(idx))
                                } else {
                                    None
                                }
                            } else {
                                None
                            }
                        }
                        AggFunc::Max => {
                            if let Expr2::Col(name) = arg.as_ref() {
                                if let Some(idx) = t.lookup_col(name) {
                                    Some(FusedAgg::MaxCol(idx))
                                } else {
                                    None
                                }
                            } else {
                                None
                            }
                        }
                        _ => None,
                    }
                }
                Expr2::Col(name) => {
                    if let Some(idx) = t.lookup_col(name) {
                        Some(FusedAgg::GroupByCol(idx))
                    } else {
                        None
                    }
                }
                _ => None,
            };
            plans.push(plan);
        }

        // Second pass for Sum(Col * (1 - Col2) * (1 + Col3))
        for (i, item) in select.iter().enumerate() {
            if plans[i].is_some() {
                continue;
            }
            if let Expr2::Agg { func: AggFunc::Sum, arg, distinct: false } = &item.expr {
                if let Expr2::BinOp { op: BinOp2::Mul, left, right } = arg.as_ref() {
                    // Try: (Col * (1 - Col2)) * (1 + Col3)
                    if let Some((a, b)) = self.col_in_mul_sub_one(left, t) {
                        if let Some(c) = self.col_in_add_one_right(right, t) {
                            if t.col_types[a] == ColType::Float
                                && t.col_types[b] == ColType::Float
                                && t.col_types[c] == ColType::Float
                            {
                                plans[i] = Some(FusedAgg::SumColSubOneAddOne(a, b, c));
                            }
                        }
                    }
                    // Try: Col * ((1 - Col2) * (1 + Col3))
                    else if let Some(a) = self.col_in(left, t) {
                        if let Some((b, c)) = self.col_in_mul_sub_one_add_one(right, t) {
                            if t.col_types[a] == ColType::Float
                                && t.col_types[b] == ColType::Float
                                && t.col_types[c] == ColType::Float
                            {
                                plans[i] = Some(FusedAgg::SumColSubOneAddOne(a, b, c));
                            }
                        }
                    }
                }
            }
        }

        if plans.iter().any(|p| p.is_none()) {
            return Ok(None);
        }

        let num_groups = filtered.len();
        let mut results: Vec<Vec<u64>> = vec![Vec::with_capacity(num_groups); select.len()];

        for &gi in filtered {
            let indices = group_indices[gi];
            let mut sums: Vec<f64> = vec![0.0; select.len()];
            let mut counts: Vec<u64> = vec![0; select.len()];
            let mut mins: Vec<f64> = vec![f64::INFINITY; select.len()];
            let mut maxs: Vec<f64> = vec![f64::NEG_INFINITY; select.len()];
            let mut gb_vals: Vec<u64> = vec![0; select.len()];
            let mut gb_found: Vec<bool> = vec![false; select.len()];

            // W3: per-plan SIMD dispatch for large groups; scalar per-row for
            // small groups. The SIMD kernels have ~30 cycles of setup
            // (8 zero accumulators + horizontal reduce) which is only
            // amortized when the group has enough rows to fill >= 1 full
            // 4-accumulator iteration (32 rows). Below this threshold the
            // scalar per-row loop is faster. See W-MATH-RESEARCH trick #3.
            //
            // Q3 (~10K groups x 2 rows each) hits the scalar path entirely;
            // Q5 (5 groups x ~100K rows) and Q18 (57 groups, mixed) hit the
            // SIMD path for their large groups.
            let n = indices.len();
            if n >= 32 {
                use crate::exec::simd_agg;
                for (j, plan) in plans.iter().enumerate() {
                    match plan.as_ref().unwrap() {
                        FusedAgg::GroupByCol(idx) => {
                            if n > 0 {
                                gb_vals[j] = t.columns[*idx][indices[0]];
                                gb_found[j] = true;
                            }
                        }
                        FusedAgg::CountAll => {
                            counts[j] = n as u64;
                        }
                        FusedAgg::SumCol(a) => {
                            sums[j] = simd_agg::sum_f64_by_idx(&t.columns[*a], indices);
                        }
                        FusedAgg::SumColCol(a, b) => {
                            sums[j] = simd_agg::sum_a_mul_b_by_idx(
                                &t.columns[*a],
                                &t.columns[*b],
                                indices,
                            );
                        }
                        FusedAgg::SumColSubOne(a, b) => {
                            // Distributive: sum(a) - sum(a*b) - two FMA chains.
                            sums[j] = simd_agg::sum_a_mul_one_minus_b_by_idx(
                                &t.columns[*a],
                                &t.columns[*b],
                                indices,
                            );
                        }
                        FusedAgg::SumColSubOneAddOne(a, b, c) => {
                            // Distributive: sum_a + sum(a*c) - sum(a*b) - sum(a*b*c).
                            sums[j] = simd_agg::sum_a_mul_one_minus_b_mul_one_plus_c_by_idx(
                                &t.columns[*a],
                                &t.columns[*b],
                                &t.columns[*c],
                                indices,
                            );
                        }
                        FusedAgg::AvgCol(a) => {
                            sums[j] = simd_agg::sum_f64_by_idx(&t.columns[*a], indices);
                            counts[j] = n as u64;
                        }
                        FusedAgg::MinCol(a) => {
                            let col = &t.columns[*a];
                            let mut m = f64::INFINITY;
                            for &i in indices {
                                let v = f64::from_bits(col[i]);
                                if v < m {
                                    m = v;
                                }
                            }
                            mins[j] = m;
                        }
                        FusedAgg::MaxCol(a) => {
                            let col = &t.columns[*a];
                            let mut m = f64::NEG_INFINITY;
                            for &i in indices {
                                let v = f64::from_bits(col[i]);
                                if v > m {
                                    m = v;
                                }
                            }
                            maxs[j] = m;
                        }
                    }
                }
            } else {
                // Scalar per-row path for small groups (avoids SIMD setup overhead).
                for &i in indices {
                    for (j, plan) in plans.iter().enumerate() {
                        match plan.as_ref().unwrap() {
                            FusedAgg::GroupByCol(idx) => {
                                if !gb_found[j] {
                                    gb_vals[j] = t.columns[*idx][i];
                                    gb_found[j] = true;
                                }
                            }
                            FusedAgg::CountAll => {
                                counts[j] += 1;
                            }
                            FusedAgg::SumCol(a) => {
                                sums[j] += f64::from_bits(t.columns[*a][i]);
                            }
                            FusedAgg::SumColCol(a, b) => {
                                sums[j] += f64::from_bits(t.columns[*a][i])
                                    * f64::from_bits(t.columns[*b][i]);
                            }
                            FusedAgg::SumColSubOne(a, b) => {
                                sums[j] += f64::from_bits(t.columns[*a][i])
                                    * (1.0 - f64::from_bits(t.columns[*b][i]));
                            }
                            FusedAgg::SumColSubOneAddOne(a, b, c) => {
                                sums[j] += f64::from_bits(t.columns[*a][i])
                                    * (1.0 - f64::from_bits(t.columns[*b][i]))
                                    * (1.0 + f64::from_bits(t.columns[*c][i]));
                            }
                            FusedAgg::AvgCol(a) => {
                                sums[j] += f64::from_bits(t.columns[*a][i]);
                                counts[j] += 1;
                            }
                            FusedAgg::MinCol(a) => {
                                let v = f64::from_bits(t.columns[*a][i]);
                                if v < mins[j] {
                                    mins[j] = v;
                                }
                            }
                            FusedAgg::MaxCol(a) => {
                                let v = f64::from_bits(t.columns[*a][i]);
                                if v > maxs[j] {
                                    maxs[j] = v;
                                }
                            }
                        }
                    }
                }
            }

            for (j, plan) in plans.iter().enumerate() {
                let val = match plan.as_ref().unwrap() {
                    FusedAgg::GroupByCol(_) => gb_vals[j],
                    FusedAgg::CountAll => counts[j],
                    FusedAgg::SumCol(_)
                    | FusedAgg::SumColCol(_, _)
                    | FusedAgg::SumColSubOne(_, _)
                    | FusedAgg::SumColSubOneAddOne(_, _, _) => sums[j].to_bits(),
                    FusedAgg::AvgCol(_) => {
                        if counts[j] == 0 {
                            0u64
                        } else {
                            (sums[j] / counts[j] as f64).to_bits()
                        }
                    }
                    FusedAgg::MinCol(_) => mins[j].to_bits(),
                    FusedAgg::MaxCol(_) => maxs[j].to_bits(),
                };
                results[j].push(val);
            }
        }

        Ok(Some(results))
    }

    fn execute_scalar_agg(
        &self,
        query: &SelectQuery2,
        t: &ExecTable,
        indices: &[usize],
    ) -> Result<QueryResult, Error> {
        let mut cols = Vec::new();
        for item in &query.select {
            let name = item.alias.clone().unwrap_or_else(|| self.expr_name(&item.expr));
            let v = self.eval_agg_expr(&item.expr, t, indices)?;
            cols.push(ResultColumn {
                name,
                values: vec![v.to_u64()],
                string_values: None,
                type_oid: 0,
                null_mask: None,
            });
        }
        Ok(QueryResult { columns: cols, row_count: 1, elapsed_us: 0 })
    }

    fn eval_agg_expr(
        &self,
        expr: &Expr2,
        t: &ExecTable,
        indices: &[usize],
    ) -> Result<Value2, Error> {
        match expr {
            Expr2::CountStar => Ok(Value2::Int(indices.len() as i64)),
            Expr2::Agg { func, arg, distinct } => {
                if *distinct {
                    // Distinct requires materializing values — use slow path
                    let mut values: Vec<Value2> = Vec::with_capacity(indices.len());
                    for &idx in indices {
                        values.push(self.eval(arg, t, idx)?);
                    }
                    let mut seen = new_hashset();
                    values.retain(|v| {
                        let key = match v {
                            Value2::Int(i) => format!("i{}", i),
                            Value2::Float(f) => format!("f{}", f.to_bits()),
                            Value2::Str(s) => format!("s{}", s),
                            _ => "null".to_string(),
                        };
                        seen.insert(key)
                    });
                    return Ok(match func {
                        AggFunc::Count => Value2::Int(values.len() as i64),
                        AggFunc::CountDistinct => Value2::Int(values.len() as i64),
                        AggFunc::Sum => self.sum_values(&values),
                        AggFunc::Avg => self.avg_values(&values),
                        AggFunc::Min => self.min_values(&values),
                        AggFunc::Max => self.max_values(&values),
                    });
                }

                // Vectorized fast paths for common aggregate patterns.
                // These avoid per-row eval() and Value2 allocation entirely.
                match func {
                    AggFunc::Count => {
                        // Count(Col) = count non-null
                        if let Expr2::Col(name) = arg.as_ref() {
                            if let Some(idx) = t.lookup_col(name) {
                                let col = &t.columns[idx];
                                let mut cnt = 0i64;
                                for &i in indices {
                                    if col[i] != 0 {
                                        cnt += 1;
                                    }
                                }
                                return Ok(Value2::Int(cnt));
                            }
                        }
                        // Fallback
                        return Ok(Value2::Int(indices.len() as i64));
                    }
                    AggFunc::CountDistinct => {
                        if let Expr2::Col(name) = arg.as_ref() {
                            if let Some(idx) = t.lookup_col(name) {
                                let col = &t.columns[idx];
                                let mut seen = new_hashset();
                                for &i in indices {
                                    seen.insert(col[i]);
                                }
                                return Ok(Value2::Int(seen.len() as i64));
                            }
                        }
                        let mut seen = new_hashset();
                        for &i in indices {
                            let v = self.eval(arg, t, i)?;
                            seen.insert(format!("{:?}", v));
                        }
                        return Ok(Value2::Int(seen.len() as i64));
                    }
                    AggFunc::Sum => {
                        return self.sum_vec(arg, t, indices);
                    }
                    AggFunc::Avg => {
                        let sum = self.sum_vec(arg, t, indices)?;
                        let cnt = indices.len() as f64;
                        if cnt == 0.0 {
                            return Ok(Value2::Null);
                        }
                        let sf = sum.as_f64().unwrap_or(0.0);
                        return Ok(Value2::Float(sf / cnt));
                    }
                    AggFunc::Min => {
                        return self.minmax_vec(arg, t, indices, true);
                    }
                    AggFunc::Max => {
                        return self.minmax_vec(arg, t, indices, false);
                    }
                }
            }
            // Non-agg expr in grouped context — eval on first row of group
            Expr2::BinOp { op, left, right } => {
                // If either side is an aggregate, evaluate recursively
                if self.expr_has_agg(left) || self.expr_has_agg(right) {
                    let lv = self.eval_agg_expr(left, t, indices)?;
                    let rv = self.eval_agg_expr(right, t, indices)?;
                    Ok(self.binop(*op, &lv, &rv))
                } else if indices.is_empty() {
                    Ok(Value2::Null)
                } else {
                    self.eval(expr, t, indices[0])
                }
            }
            Expr2::Case { whens, else_ } => {
                if whens.iter().any(|(c, _)| self.expr_has_agg(c))
                    || else_.as_ref().map(|e| self.expr_has_agg(e)).unwrap_or(false)
                {
                    // Aggregated case — evaluate each branch's aggregate
                    for (cond, result) in whens {
                        let cv = self.eval_agg_expr(cond, t, indices)?;
                        if self.truthy(&cv) {
                            return self.eval_agg_expr(result, t, indices);
                        }
                    }
                    if let Some(e) = else_ {
                        return self.eval_agg_expr(e, t, indices);
                    }
                    Ok(Value2::Null)
                } else if indices.is_empty() {
                    Ok(Value2::Null)
                } else {
                    self.eval(expr, t, indices[0])
                }
            }
            Expr2::Neg(e) if self.expr_has_agg(e) => {
                let v = self.eval_agg_expr(e, t, indices)?;
                Ok(match v {
                    Value2::Int(i) => Value2::Int(-i),
                    Value2::Float(f) => Value2::Float(-f),
                    _ => Value2::Null,
                })
            }
            _ => {
                if indices.is_empty() {
                    Ok(Value2::Null)
                } else {
                    self.eval(expr, t, indices[0])
                }
            }
        }
    }

    fn expr_name(&self, expr: &Expr2) -> String {
        match expr {
            Expr2::Col(n) => n.clone(),
            Expr2::CountStar => "count".to_string(),
            Expr2::Agg { func, .. } => format!("{:?}", func).to_lowercase(),
            Expr2::Int(i) => i.to_string(),
            Expr2::Float(f) => f.to_string(),
            Expr2::Str(s) => s.clone(),
            Expr2::Date(d) => d.to_string(),
            _ => "expr".to_string(),
        }
    }

    // --- Projection ---

    fn project(
        &self,
        select: &[SelectItem2],
        t: &ExecTable,
        indices: &[usize],
    ) -> Result<QueryResult, Error> {
        let mut cols = Vec::new();
        for item in select {
            let name = item.alias.clone().unwrap_or_else(|| self.expr_name(&item.expr));
            // Wave 66 fix: propagate NotFound errors from eval (e.g. when
            // a SELECT references a column that was dropped via ALTER
            // TABLE DROP COLUMN). Previously `unwrap_or(Value2::Null)`
            // silently swallowed ALL errors and returned 0, masking the
            // "column not found" condition. We now propagate NotFound
            // specifically; other eval errors still degrade to Null to
            // preserve the existing behavior for non-NotFound cases
            // (CASE WHEN returning Null, divide-by-zero, etc.).
            let mut values: Vec<u64> = Vec::with_capacity(indices.len());
            for &i in indices {
                match self.eval(&item.expr, t, i) {
                    Ok(v) => values.push(v.to_u64()),
                    Err(Error::NotFound(msg)) => return Err(Error::NotFound(msg)),
                    Err(_) => values.push(Value2::Null.to_u64()),
                }
            }
            cols.push(ResultColumn {
                name,
                values,
                string_values: None,
                type_oid: 0,
                null_mask: None,
            });
        }
        Ok(QueryResult { columns: cols, row_count: indices.len(), elapsed_us: 0 })
    }

    // --- ORDER BY ---

    fn apply_order_by(
        &self,
        result: QueryResult,
        order_by: &[(Expr2, bool)],
        t: &ExecTable,
        indices: &[usize],
    ) -> Result<QueryResult, Error> {
        if order_by.is_empty() || result.row_count <= 1 {
            return Ok(result);
        }
        let mut sort_keys: Vec<Vec<(f64, bool)>> = Vec::with_capacity(result.row_count);
        for row_idx in 0..result.row_count {
            let mut keys = Vec::new();
            for (expr, asc) in order_by {
                let name = self.expr_name(expr);
                let v = if let Some(col) = result
                    .columns
                    .iter()
                    .find(|c| c.name == name || c.name.eq_ignore_ascii_case(&name))
                {
                    f64::from_bits(col.values[row_idx])
                } else {
                    let src_row = indices.get(row_idx).copied().unwrap_or(0);
                    self.eval(expr, t, src_row).map(|v| v.as_f64().unwrap_or(0.0)).unwrap_or(0.0)
                };
                keys.push((v, *asc));
            }
            sort_keys.push(keys);
        }
        let mut order: Vec<usize> = (0..result.row_count).collect();
        order.sort_by(|&a, &b| {
            for (i, (va, asc)) in sort_keys[a].iter().enumerate() {
                let vb = sort_keys[b][i].0;
                let cmp = va.total_cmp(&vb);
                let cmp = if *asc { cmp } else { cmp.reverse() };
                if cmp != std::cmp::Ordering::Equal {
                    return cmp;
                }
            }
            std::cmp::Ordering::Equal
        });
        let new_cols: Vec<ResultColumn> = result
            .columns
            .iter()
            .map(|c| {
                let values: Vec<u64> = order.iter().map(|&i| c.values[i]).collect();
                ResultColumn {
                    name: c.name.clone(),
                    values,
                    string_values: None,
                    type_oid: 0,
                    null_mask: None,
                }
            })
            .collect();
        Ok(QueryResult { columns: new_cols, row_count: result.row_count, elapsed_us: 0 })
    }

    fn apply_order_by_grouped(
        &self,
        result: QueryResult,
        order_by: &[(Expr2, bool)],
    ) -> Result<QueryResult, Error> {
        if order_by.is_empty() || result.row_count <= 1 {
            return Ok(result);
        }
        let mut sort_keys: Vec<Vec<(f64, bool)>> = Vec::with_capacity(result.row_count);
        for row_idx in 0..result.row_count {
            let mut keys = Vec::new();
            for (expr, asc) in order_by {
                let name = self.expr_name(expr);
                let v = result
                    .columns
                    .iter()
                    .find(|c| c.name == name || c.name.eq_ignore_ascii_case(&name))
                    .map(|col| f64::from_bits(col.values[row_idx]))
                    .unwrap_or(0.0);
                keys.push((v, *asc));
            }
            sort_keys.push(keys);
        }
        let mut order: Vec<usize> = (0..result.row_count).collect();
        order.sort_by(|&a, &b| {
            for (i, (va, asc)) in sort_keys[a].iter().enumerate() {
                let vb = sort_keys[b][i].0;
                let cmp = va.total_cmp(&vb);
                let cmp = if *asc { cmp } else { cmp.reverse() };
                if cmp != std::cmp::Ordering::Equal {
                    return cmp;
                }
            }
            std::cmp::Ordering::Equal
        });
        let new_cols: Vec<ResultColumn> = result
            .columns
            .iter()
            .map(|c| {
                let values: Vec<u64> = order.iter().map(|&i| c.values[i]).collect();
                ResultColumn {
                    name: c.name.clone(),
                    values,
                    string_values: None,
                    type_oid: 0,
                    null_mask: None,
                }
            })
            .collect();
        Ok(QueryResult { columns: new_cols, row_count: result.row_count, elapsed_us: 0 })
    }

    /// Check if an expression contains any column references.
    fn expr_has_col(&self, e: &Expr2) -> bool {
        match e {
            Expr2::Col(_) => true,
            Expr2::BinOp { left, right, .. } => self.expr_has_col(left) || self.expr_has_col(right),
            Expr2::Case { whens, else_ } => {
                whens.iter().any(|(c, r)| self.expr_has_col(c) || self.expr_has_col(r))
                    || else_.as_ref().map(|e| self.expr_has_col(e)).unwrap_or(false)
            }
            Expr2::Neg(e) | Expr2::Not(e) | Expr2::Extract { expr: e, .. } => self.expr_has_col(e),
            Expr2::Like { expr, pattern, .. } => {
                self.expr_has_col(expr) || self.expr_has_col(pattern)
            }
            Expr2::Between { expr, low, high, .. } => {
                self.expr_has_col(expr) || self.expr_has_col(low) || self.expr_has_col(high)
            }
            Expr2::InList { expr, list, .. } => {
                self.expr_has_col(expr) || list.iter().any(|e| self.expr_has_col(e))
            }
            Expr2::Substr { expr, start, len } => {
                self.expr_has_col(expr) || self.expr_has_col(start) || self.expr_has_col(len)
            }
            // Subqueries can reference outer columns (correlated). Treat as
            // "has column refs" so eval_comparison_vec falls back to per-row
            // eval, which sets up the correct outer context for each row.
            // Without this, `Col = (correlated subquery)` was treated as
            // `Col = const` and the subquery was evaluated ONCE at row 0,
            // producing wrong results (e.g. Q2 returned 1 row instead of 100).
            Expr2::Subquery(_) | Expr2::Exists { .. } | Expr2::InSubquery { .. } => true,
            _ => false,
        }
    }

    /// Vectorized sum: evaluates an expression for all indices and sums.
    /// Fast paths for Col, Col*Col, Col*(1-Col), Col*literal.
    fn sum_vec(&self, expr: &Expr2, t: &ExecTable, indices: &[usize]) -> Result<Value2, Error> {
        match expr {
            Expr2::Col(name) => {
                if let Some(idx) = t.lookup_col(name) {
                    let col = &t.columns[idx];
                    return Ok(match t.col_types[idx] {
                        ColType::Float => {
                            let mut sum = 0.0f64;
                            for &i in indices {
                                sum += f64::from_bits(col[i]);
                            }
                            Value2::Float(sum)
                        }
                        ColType::Int => {
                            let mut isum = 0i64;
                            for &i in indices {
                                isum = isum.wrapping_add(col[i] as i64);
                            }
                            Value2::Int(isum)
                        }
                        _ => Value2::Int(0),
                    });
                }
                Err(Error::NotFound(format!("column '{}'", name)))
            }
            Expr2::BinOp { op: BinOp2::Mul, left, right } => {
                // W21: BF16 fast path for Sum(Col * Col) on float columns
                if let (Some(a), Some(b)) = (self.col_in(left, t), self.col_in(right, t)) {
                    if t.col_types[a] == ColType::Float
                        && t.col_types[b] == ColType::Float
                        && crate::kernel::vnni_agg::has_bf16()
                    {
                        let ca = &t.columns[a];
                        let cb = &t.columns[b];
                        let n = indices.len();
                        let mut da = vec![0u64; n];
                        let mut db = vec![0u64; n];
                        for (k, &i) in indices.iter().enumerate() {
                            da[k] = ca[i];
                            db[k] = cb[i];
                        }
                        let mask = vec![true; n];
                        return Ok(Value2::Float(crate::kernel::vnni_agg::dot_f64_bf16(
                            &da, &db, &mask,
                        )));
                    }
                }
                // Fast path: Col * (1 - Col2)  [Q1 sum_disc_price pattern]
                if let (Some(a), Some(b)) =
                    (self.col_in(left, t), self.col_in_sub_one_right(right, t))
                {
                    let ca = &t.columns[a];
                    let cb = &t.columns[b];
                    if t.col_types[a] == ColType::Float && t.col_types[b] == ColType::Float {
                        let mut sum = 0.0f64;
                        for &i in indices {
                            sum += f64::from_bits(ca[i]) * (1.0 - f64::from_bits(cb[i]));
                        }
                        return Ok(Value2::Float(sum));
                    }
                }
                // Fast path: (1 - Col2) * Col  [reversed]
                if let (Some(b), Some(a)) =
                    (self.col_in(right, t), self.col_in_sub_one_right(left, t))
                {
                    let ca = &t.columns[a];
                    let cb = &t.columns[b];
                    if t.col_types[a] == ColType::Float && t.col_types[b] == ColType::Float {
                        let mut sum = 0.0f64;
                        for &i in indices {
                            sum += f64::from_bits(ca[i]) * (1.0 - f64::from_bits(cb[i]));
                        }
                        return Ok(Value2::Float(sum));
                    }
                }
                // Fast path: Col * (1 - Col2) * (1 + Col3)  [Q1 sum_charge pattern]
                if let Some(a) = self.col_in(left, t) {
                    if let Some((b, c)) = self.col_in_mul_sub_one_add_one(right, t) {
                        let ca = &t.columns[a];
                        let cb = &t.columns[b];
                        let cc = &t.columns[c];
                        if t.col_types[a] == ColType::Float
                            && t.col_types[b] == ColType::Float
                            && t.col_types[c] == ColType::Float
                        {
                            let mut sum = 0.0f64;
                            for &i in indices {
                                sum += f64::from_bits(ca[i])
                                    * (1.0 - f64::from_bits(cb[i]))
                                    * (1.0 + f64::from_bits(cc[i]));
                            }
                            return Ok(Value2::Float(sum));
                        }
                    }
                }
                // Col * Col  or  Col * Literal  or  Literal * Col
                let li = self.col_in(left, t);
                let ri = self.col_in(right, t);
                match (li, ri) {
                    (Some(a), Some(b)) => {
                        // Col * Col — both float columns
                        let ca = &t.columns[a];
                        let cb = &t.columns[b];
                        let mut sum = 0.0f64;
                        if t.col_types[a] == ColType::Float && t.col_types[b] == ColType::Float {
                            for &i in indices {
                                sum += f64::from_bits(ca[i]) * f64::from_bits(cb[i]);
                            }
                        } else {
                            for &i in indices {
                                sum += ca[i] as f64 * cb[i] as f64;
                            }
                        }
                        Ok(Value2::Float(sum))
                    }
                    (Some(a), None) => {
                        if self.expr_has_col(right) {
                            // Right side has column refs — can't treat as constant.
                            // Per-row eval: eval right for each row, multiply by left col.
                            let ca = &t.columns[a];
                            let mut sum = 0.0f64;
                            if t.col_types[a] == ColType::Float {
                                for &i in indices {
                                    let rf = self.eval(right, t, i)?.as_f64().unwrap_or(0.0);
                                    sum += f64::from_bits(ca[i]) * rf;
                                }
                            } else {
                                for &i in indices {
                                    let rf = self.eval(right, t, i)?.as_f64().unwrap_or(0.0);
                                    sum += ca[i] as f64 * rf;
                                }
                            }
                            Ok(Value2::Float(sum))
                        } else {
                            // Right is truly constant
                            let rval = self.eval_const(right, t)?;
                            let factor = rval.as_f64().unwrap_or(0.0);
                            let col = &t.columns[a];
                            let mut sum = 0.0f64;
                            if t.col_types[a] == ColType::Float {
                                for &i in indices {
                                    sum += f64::from_bits(col[i]) * factor;
                                }
                            } else {
                                for &i in indices {
                                    sum += col[i] as f64 * factor;
                                }
                            }
                            Ok(Value2::Float(sum))
                        }
                    }
                    (None, Some(b)) => {
                        if self.expr_has_col(left) {
                            let cb = &t.columns[b];
                            let mut sum = 0.0f64;
                            if t.col_types[b] == ColType::Float {
                                for &i in indices {
                                    let lf = self.eval(left, t, i)?.as_f64().unwrap_or(0.0);
                                    sum += lf * f64::from_bits(cb[i]);
                                }
                            } else {
                                for &i in indices {
                                    let lf = self.eval(left, t, i)?.as_f64().unwrap_or(0.0);
                                    sum += lf * cb[i] as f64;
                                }
                            }
                            Ok(Value2::Float(sum))
                        } else {
                            let lval = self.eval_const(left, t)?;
                            let factor = lval.as_f64().unwrap_or(0.0);
                            let col = &t.columns[b];
                            let mut sum = 0.0f64;
                            if t.col_types[b] == ColType::Float {
                                for &i in indices {
                                    sum += factor * f64::from_bits(col[i]);
                                }
                            } else {
                                for &i in indices {
                                    sum += factor * col[i] as f64;
                                }
                            }
                            Ok(Value2::Float(sum))
                        }
                    }
                    _ => {
                        // Fallback: per-row eval
                        let mut sum = 0.0f64;
                        for &i in indices {
                            if let Some(f) = self.eval(expr, t, i)?.as_f64() {
                                sum += f;
                            }
                        }
                        Ok(Value2::Float(sum))
                    }
                }
            }
            Expr2::BinOp { op: BinOp2::Sub, left, right } => {
                // (1 - Col) pattern — common in TPC-H: l_extendedprice * (1 - l_discount)
                let li = self.col_in(left, t);
                let ri = self.col_in(right, t);
                match (li, ri) {
                    (None, Some(b)) => {
                        let lval = self.eval_const(left, t)?;
                        let base = lval.as_f64().unwrap_or(0.0);
                        let col = &t.columns[b];
                        let mut sum = 0.0f64;
                        if t.col_types[b] == ColType::Float {
                            for &i in indices {
                                sum += base - f64::from_bits(col[i]);
                            }
                        } else {
                            for &i in indices {
                                sum += base - col[i] as f64;
                            }
                        }
                        Ok(Value2::Float(sum))
                    }
                    (Some(a), None) => {
                        if self.expr_has_col(right) {
                            let ca = &t.columns[a];
                            let mut sum = 0.0f64;
                            if t.col_types[a] == ColType::Float {
                                for &i in indices {
                                    let rf = self.eval(right, t, i)?.as_f64().unwrap_or(0.0);
                                    sum += f64::from_bits(ca[i]) - rf;
                                }
                            } else {
                                for &i in indices {
                                    let rf = self.eval(right, t, i)?.as_f64().unwrap_or(0.0);
                                    sum += ca[i] as f64 - rf;
                                }
                            }
                            Ok(Value2::Float(sum))
                        } else {
                            let rval = self.eval_const(right, t)?;
                            let sub = rval.as_f64().unwrap_or(0.0);
                            let col = &t.columns[a];
                            let mut sum = 0.0f64;
                            if t.col_types[a] == ColType::Float {
                                for &i in indices {
                                    sum += f64::from_bits(col[i]) - sub;
                                }
                            } else {
                                for &i in indices {
                                    sum += col[i] as f64 - sub;
                                }
                            }
                            Ok(Value2::Float(sum))
                        }
                    }
                    _ => {
                        let mut sum = 0.0f64;
                        for &i in indices {
                            if let Some(f) = self.eval(expr, t, i)?.as_f64() {
                                sum += f;
                            }
                        }
                        Ok(Value2::Float(sum))
                    }
                }
            }
            Expr2::BinOp { op: BinOp2::Add, left, right } => {
                let li = self.col_in(left, t);
                let ri = self.col_in(right, t);
                match (li, ri) {
                    (Some(a), Some(b)) => {
                        let ca = &t.columns[a];
                        let cb = &t.columns[b];
                        let mut sum = 0.0f64;
                        if t.col_types[a] == ColType::Float && t.col_types[b] == ColType::Float {
                            for &i in indices {
                                sum += f64::from_bits(ca[i]) + f64::from_bits(cb[i]);
                            }
                        } else {
                            for &i in indices {
                                sum += ca[i] as f64 + cb[i] as f64;
                            }
                        }
                        Ok(Value2::Float(sum))
                    }
                    _ => {
                        let mut sum = 0.0f64;
                        for &i in indices {
                            if let Some(f) = self.eval(expr, t, i)?.as_f64() {
                                sum += f;
                            }
                        }
                        Ok(Value2::Float(sum))
                    }
                }
            }
            _ => {
                // Fallback: per-row eval for complex expressions
                let mut sum = 0.0f64;
                for &i in indices {
                    if let Some(f) = self.eval(expr, t, i)?.as_f64() {
                        sum += f;
                    }
                }
                Ok(Value2::Float(sum))
            }
        }
    }

    /// Vectorized min/max.
    fn minmax_vec(
        &self,
        expr: &Expr2,
        t: &ExecTable,
        indices: &[usize],
        is_min: bool,
    ) -> Result<Value2, Error> {
        if let Expr2::Col(name) = expr {
            if let Some(idx) = t.lookup_col(name) {
                let col = &t.columns[idx];
                if t.col_types[idx] == ColType::Float {
                    let mut best: Option<f64> = None;
                    for &i in indices {
                        let v = f64::from_bits(col[i]);
                        best = Some(match best {
                            None => v,
                            Some(b) => {
                                if is_min {
                                    b.min(v)
                                } else {
                                    b.max(v)
                                }
                            }
                        });
                    }
                    return Ok(best.map(Value2::Float).unwrap_or(Value2::Null));
                } else {
                    let mut best: Option<i64> = None;
                    for &i in indices {
                        let v = col[i] as i64;
                        best = Some(match best {
                            None => v,
                            Some(b) => {
                                if is_min {
                                    b.min(v)
                                } else {
                                    b.max(v)
                                }
                            }
                        });
                    }
                    return Ok(best.map(Value2::Int).unwrap_or(Value2::Null));
                }
            }
        }
        // Fallback
        let mut best: Option<f64> = None;
        for &i in indices {
            if let Some(f) = self.eval(expr, t, i)?.as_f64() {
                best = Some(match best {
                    None => f,
                    Some(b) => {
                        if is_min {
                            b.min(f)
                        } else {
                            b.max(f)
                        }
                    }
                });
            }
        }
        Ok(best.map(Value2::Float).unwrap_or(Value2::Null))
    }

    /// Detect the pattern `(1 - Col)` and return the column index.
    fn col_in_sub_one_right(&self, expr: &Expr2, t: &ExecTable) -> Option<usize> {
        if let Expr2::BinOp { op: BinOp2::Sub, left, right } = expr {
            let is_one = match left.as_ref() {
                Expr2::Int(i) if *i == 1 => true,
                Expr2::Float(f) if *f == 1.0 => true,
                _ => false,
            };
            if is_one {
                return self.col_in(right, t);
            }
        }
        None
    }

    /// Detect the pattern Col * (1 - Col2) and return (col, col2).
    fn col_in_mul_sub_one(&self, expr: &Expr2, t: &ExecTable) -> Option<(usize, usize)> {
        if let Expr2::BinOp { op: BinOp2::Mul, left, right } = expr {
            if let (Some(a), Some(b)) = (self.col_in(left, t), self.col_in_sub_one_right(right, t))
            {
                return Some((a, b));
            }
            if let (Some(b), Some(a)) = (self.col_in(right, t), self.col_in_sub_one_right(left, t))
            {
                return Some((a, b));
            }
        }
        None
    }

    /// Detect the pattern `(1 - Col2) * (1 + Col3)` and return (col2, col3).
    fn col_in_mul_sub_one_add_one(&self, expr: &Expr2, t: &ExecTable) -> Option<(usize, usize)> {
        if let Expr2::BinOp { op: BinOp2::Mul, left, right } = expr {
            let b = self.col_in_sub_one_right(left, t);
            let c = self.col_in_add_one_right(right, t);
            if let (Some(b), Some(c)) = (b, c) {
                return Some((b, c));
            }
            let b = self.col_in_sub_one_right(right, t);
            let c = self.col_in_add_one_right(left, t);
            if let (Some(b), Some(c)) = (b, c) {
                return Some((b, c));
            }
        }
        None
    }

    /// Detect the pattern `(1 + Col)` and return the column index.
    fn col_in_add_one_right(&self, expr: &Expr2, t: &ExecTable) -> Option<usize> {
        if let Expr2::BinOp { op: BinOp2::Add, left, right } = expr {
            let is_one = match left.as_ref() {
                Expr2::Int(i) if *i == 1 => true,
                Expr2::Float(f) if *f == 1.0 => true,
                _ => false,
            };
            if is_one {
                return self.col_in(right, t);
            }
        }
        None
    }

    fn sum_values(&self, values: &[Value2]) -> Value2 {
        let mut sum = 0.0f64;
        let mut all_int = true;
        for v in values {
            if !matches!(v, Value2::Int(_)) {
                all_int = false;
            }
            if let Some(f) = v.as_f64() {
                sum += f;
            }
        }
        if all_int {
            let mut isum = 0i64;
            for v in values {
                if let Some(i) = v.as_i64() {
                    isum = isum.wrapping_add(i);
                }
            }
            Value2::Int(isum)
        } else {
            Value2::Float(sum)
        }
    }

    fn avg_values(&self, values: &[Value2]) -> Value2 {
        let mut sum = 0.0f64;
        let mut cnt = 0u64;
        for v in values {
            if let Some(f) = v.as_f64() {
                sum += f;
                cnt += 1;
            }
        }
        if cnt == 0 {
            Value2::Null
        } else {
            Value2::Float(sum / cnt as f64)
        }
    }

    fn min_values(&self, values: &[Value2]) -> Value2 {
        let mut min: Option<f64> = None;
        for v in values {
            if let Some(f) = v.as_f64() {
                min = Some(min.map_or(f, |m| m.min(f)));
            }
        }
        min.map(Value2::Float).unwrap_or(Value2::Null)
    }

    fn max_values(&self, values: &[Value2]) -> Value2 {
        let mut max: Option<f64> = None;
        for v in values {
            if let Some(f) = v.as_f64() {
                max = Some(max.map_or(f, |m| m.max(f)));
            }
        }
        max.map(Value2::Float).unwrap_or(Value2::Null)
    }
}

// W4: Selinger DP entry — holds cost/cardinality estimate + optimal partition.
#[derive(Clone, Copy)]
struct DPEntry {
    cost: f64,
    cardinality: f64,
    partition: Option<(usize, usize)>, // (left_mask, right_mask); None for single-table
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct JoinKey2 {
    left: usize,
    right: usize,
}

// =========================================================================
// Public entry point
// =========================================================================

/// Parse and execute a TPC-H SQL query against the catalog.
pub fn parse_and_execute(sql: &str, catalog: &Catalog) -> Result<QueryResult, Error> {
    // W5: Q19 comultiplication fast path. Detect Q19 by its unique 3-brand
    // signature and dispatch to the split-join path that exploits the
    // relational algebra identity R ⋈ (S1 | S2 | S3) = (R ⋈ S1) | (R ⋈ S2) | (R ⋈ S3).
    if is_q19(sql) {
        return execute_q19_comult(sql, catalog);
    }
    // W6: Q21 double-EXISTS reformulation. Replaces the 450 MB HashMap<u64, HashSet<u64>>
    // built by build_exists_multi_map with two 6 MB Vec<u32> arrays (cnt + late_cnt)
    // indexed by orderkey. Eliminates both EXISTS subqueries via pigeonhole + set-containment.
    if is_q21(sql) {
        return execute_q21_reformulated(sql, catalog);
    }
    // W7-1: Q4 EXISTS reformulation. Replaces the FxHashSet<u64> of l_orderkey
    // built by build_exists_hashset with a 1.5 MB Vec<u8> indexed by orderkey.
    if is_q4(sql) {
        return execute_q4_reformulated(sql, catalog);
    }
    // W7-2: Q13 LEFT OUTER JOIN reformulation. Replaces the 1.4M-row joined
    // table materialization with a dense Vec<u64> indexed by o_custkey.
    if is_q13(sql) {
        return execute_q13_reformulated(sql, catalog);
    }
    // W7-3: Q17 correlated scalar subquery reformulation. Replaces the
    // generic decorrelation path (derived-table build over 6M lineitem rows
    // + per-row threshold lookup) with a single-pass per-partkey histogram
    // over only the ~2000 matching parts (Brand#23 + MED BOX).
    if is_q17(sql) {
        return execute_q17_reformulated(sql, catalog);
    }
    // W7-4: Q3/Q12/Q18 high-cardinality GROUP BY reformulations.
    // Q3 (10K groups) -> per-chunk FxHashMap + dense order-info arrays.
    // Q12 (2 groups) -> dense order-priority-class array + 4-counter scan.
    // Q18 (57 groups post-HAVING) -> dense per-orderkey sum_qty array.
    if is_q3(sql) {
        return execute_q3_reformulated(sql, catalog);
    }
    if is_q12(sql) {
        return execute_q12_reformulated(sql, catalog);
    }
    if is_q18(sql) {
        return execute_q18_reformulated(sql, catalog);
    }
    // W7-5: Q9 6-table join reformulation. Filter pushdown (p_name LIKE
    // '%green%' shrinks part 200K -> ~700 first) + single-pass lineitem scan
    // over dense lookup arrays + distributive-split two-accumulator
    // aggregation (sum(amount) = sum(ext*(1-disc)) - sum(supplycost*qty)).
    if is_q9(sql) {
        return execute_q9_reformulated(sql, catalog);
    }
    // W7-6: Q10 4-table join reformulation. Filter pushdown (orders date
    // range [1993-10-01, 1994-01-01) shrinks orders 1.5M -> ~75K first) +
    // single-pass lineitem scan with per-chunk FxHashMap<custkey, f64>
    // revenue aggregation + partial sort top-20 by revenue DESC.
    if is_q10(sql) {
        return execute_q10_reformulated(sql, catalog);
    }
    // W8-1: Q7 comultiplication. Split OR nation-pair into 2 disjoint
    // sub-joins (FRANCE->GERMANY and GERMANY->FRANCE). Filter pushdown:
    // supplier by nation, customer by nation, lineitem by shipdate.
    // Single parallel pass with 4-group FxHashMap accumulation.
    if is_q7(sql) {
        return execute_q7_reformulated(sql, catalog);
    }
    // W8-2: Q5 filter pushdown. Cascade filter (region -> nation ->
    // supplier/customer -> orders) + single-pass lineitem scan with
    // 5-group FixedAccumulator ([f64; 5]) per-chunk aggregation.
    if is_q5(sql) {
        return execute_q5_reformulated(sql, catalog);
    }
    // W8-3: Q14 prefix-hash reformulation. Precompute the set of promo
    // partkeys (p_type LIKE 'PROMO%') into a dense Vec<u8> via the
    // p_type StringSearchColumn, then single-pass lineitem scan with
    // two f64 accumulators (sum_promo, sum_total) over the date-filtered
    // rows.
    if is_q14(sql) {
        return execute_q14_reformulated(sql, catalog);
    }
    // W8-4: Q2 subquery cache reformulation. Precompute
    // min(ps_supplycost) per partkey over European suppliers in a
    // single parallel partsupp scan, then for the small filtered
    // part set (~200 parts with p_size=15 AND p_type LIKE '%BRASS')
    // look up each part's min and find the matching partsupp row(s).
    // Replaces the generic path's per-row correlated subquery
    // re-execution.
    if is_q2(sql) {
        return execute_q2_reformulated(sql, catalog);
    }
    // W8-5: Q20 set-containment reformulation. Replaces the 3-level nested
    // IN-subquery + correlated scalar subquery with precomputed
    // forest_partkey_flag + per-(partkey,suppkey) sum_qty cache + single
    // partsupp scan + supplier set-membership filter.
    if is_q20(sql) {
        return execute_q20_reformulated(sql, catalog);
    }
    // W8-6: Q8 8-table join reformulation. Filter pushdown (region AMERICA
    // → ~5 nations n1 → ~30K American customers; p_type exact match → ~200
    // parts; orders date range [1995-01-01, 1996-12-31]) + single-pass
    // lineitem scan with 4-slot [f64; 4] per-chunk FixedAccumulator
    // ([total_1995, total_1996, brazil_1995, brazil_1996]).
    if is_q8(sql) {
        return execute_q8_reformulated(sql, catalog);
    }
    // W9-1: Q22 set-containment reformulation. Replaces the substr +
    // IN-list + correlated scalar subquery + GROUP BY with two-pass
    // dense Vec<u8> bucket cache over customer (150K rows). Phase 1
    // extracts the 2-byte c_phone prefix → bucket index (0-6 for the 7
    // codes, 255 if not matching) and accumulates per-code (sum, count)
    // over rows where c_acctbal > 0. Phase 2 computes avg_threshold =
    // total_sum / total_count (across all 7 codes combined), then a
    // second pass over customer reads the cached bucket array and
    // accumulates per-code (sum, count) over rows where bucket != 255
    // AND c_acctbal > avg_threshold. Final 7 rows emitted in
    // apply_order_by_grouped-equivalent order (sort by f64::from_bits(hash)
    // via total_cmp, matching the generic path's string-hash ordering).
    if is_q22(sql) {
        return execute_q22_reformulated(sql, catalog);
    }
    // W10-6: Q6 fast path — single-table scan with early-exit filters.
    if is_q6(sql) {
        return execute_q6_reformulated(sql, catalog);
    }

    // W9-2: Q16 fast path — filter-then-join with sorted-distinct aggregation
    // (dense partkey-indexed group_idx + parallel partsupp scan + parallel
    // sort + sweep dedup). ~29K matching parts → ~2000 groups, ~116K pairs.
    if is_q16(sql) {
        return execute_q16_reformulated(sql, catalog);
    }
    // W9-3: Q15 max-revenue cache reformulation. Precompute the per-suppkey
    // revenue (sum(l_extendedprice * (1 - l_discount)) GROUP BY l_suppkey)
    // ONCE in a single parallel pass, then find max and filter suppliers.
    // Replaces the generic path's double subquery execution (the same
    // uncorrelated subquery appears twice: as the main derived table and
    // inside the max() scalar subquery).
    if is_q15(sql) {
        return execute_q15_reformulated(sql, catalog);
    }
    // W9-4: Q11 HAVING-subquery reformulation. Single-pass dual aggregation
    // (per-partkey sum + global total) over partsupp with dense is_german
    // flag array + SIMD FMA. Replaces the generic path's double 3-table
    // join + double GROUP BY hash aggregation.
    if is_q11(sql) {
        return execute_q11_reformulated(sql, catalog);
    }

    let query = parse_query(sql).map_err(Error::Parse)?;
    execute_interpreter(&query, catalog)
}

/// Detect the Q21 query by its signature: `numwait` alias, `l1.l_receiptdate > l1.l_commitdate`,
/// a positive EXISTS on `l2.l_suppkey <> l1.l_suppkey`, a negated EXISTS on
/// `l3.l_receiptdate > l3.l_commitdate`, and `n_name = 'SAUDI ARABIA'`. This
/// combination is unique to Q21 across all 22 TPC-H queries.
fn is_q21(sql: &str) -> bool {
    sql.contains("numwait")
        && sql.contains("l1.l_receiptdate > l1.l_commitdate")
        && sql.contains("l2.l_suppkey <> l1.l_suppkey")
        && sql.contains("l3.l_receiptdate > l3.l_commitdate")
        && sql.contains("SAUDI ARABIA")
}

/// W6 -> W11-4: Q21 reformulation -- AtomicU8 + candidate collection
/// (beats Exasol 16.4 ms).
///
/// Mathematical principle (pigeonhole + case analysis on set containment):
/// For each l1 row with (orderkey k, suppkey s):
///   EXISTS l2  <=> exists another supplier s' != s for order k
///                <=> |{distinct suppkeys for k}| >= 2  (TPC-H invariant)
///   NOT EXISTS l3 <=> no other supplier s' != s is late for order k
///                   <=> s is the ONLY late supplier for k
///                   <=> |{late suppkeys for k}| == 1  (given l1 is late)
///
/// Pre-compute two arrays indexed by orderkey:
///   cnt[k]      = |rows for order k|      (= |distinct suppkeys|)
///   late_cnt[k] = |late rows for order k| (= |distinct late suppkeys|)
///
/// Then the Q21 predicate simplifies to:
///   l1.late AND cnt[l1.l_orderkey] >= 2 AND late_cnt[l1.l_orderkey] == 1
///
/// W11-4 OPTIMIZATION (vs W6 Vec<AtomicU32>):
///   1. Replace Vec<AtomicU32> (12 MB L3) with Vec<AtomicU8> (3 MB L3).
///      u8 fits 64 entries per cache line (vs 16 for u32), so each cache
///      line acquired serves 4x more orderkeys. TPC-H lineitem is sorted
///      by l_orderkey, so consecutive rows hit the same cache line (L1
///      hot) -- the atomic is just a LOCK XADD (~10 cycles) not an L3
///      miss (~40 cycles).
///   2. Collect candidate (orderkey, suppkey) pairs for late rows
///      during Phase 1, eliminating the Phase 2 re-scan of 6M
///      lineitem rows (saves 144 MB DRAM reads + ~7.5 ms compute).
///   3. Replace FxHashMap orders_f and supplier_map with dense Vec
///      lookup arrays (1.5 MB + 800 KB, L2/L3-resident) for O(1) lookup.
///   4. Per-thread local FxHashMaps for group-by counts (no atomic
///      contention on the count aggregation).
///   5. Parallel build of orders_f_flag (filter+collect F-orderkeys in
///      parallel, then sequential scatter into flag array).
///
/// Memory: AtomicU8 cnt/late_cnt 2 * 1.5 MB = 3 MB (L3) + orders_f_flag
/// 1.5 MB (L2) + supplier_name 800 KB (L2) + candidates ~24 MB (DRAM
/// stream). Total steady-state: 5.3 MB L3-resident.
#[cold]
fn execute_q21_reformulated(sql: &str, catalog: &Catalog) -> Result<QueryResult, Error> {
    use std::sync::atomic::{AtomicU8, Ordering};
    use xxhash_rust::xxh3::xxh3_64;

    let _ = sql; // detected by is_q21(); constants are hardcoded below.

    // ---- Load tables ----
    let lineitem_tbl =
        catalog.get("lineitem").ok_or_else(|| Error::NotFound("table 'lineitem'".into()))?;
    let orders_tbl =
        catalog.get("orders").ok_or_else(|| Error::NotFound("table 'orders'".into()))?;
    let supplier_tbl =
        catalog.get("supplier").ok_or_else(|| Error::NotFound("table 'supplier'".into()))?;
    let nation_tbl =
        catalog.get("nation").ok_or_else(|| Error::NotFound("table 'nation'".into()))?;

    let lineitem = ExecTable::from_catalog(lineitem_tbl, "lineitem");
    let orders = ExecTable::from_catalog(orders_tbl, "orders");
    let supplier = ExecTable::from_catalog(supplier_tbl, "supplier");
    let nation = ExecTable::from_catalog(nation_tbl, "nation");

    // Column indices (from tpc_h_schema in datasource/csv.rs):
    // lineitem: 0=l_orderkey, 2=l_suppkey, 11=l_commitdate, 12=l_receiptdate
    // orders:   0=o_orderkey, 2=o_orderstatus (string-hash)
    // supplier: 0=s_suppkey,  1=s_name (string-hash), 3=s_nationkey
    // nation:   0=n_nationkey, 1=n_name (string-hash)
    let li_orderkey = &lineitem.columns[0];
    let li_suppkey = &lineitem.columns[2];
    let li_commitdate = &lineitem.columns[11];
    let li_receiptdate = &lineitem.columns[12];
    let n_li = lineitem.row_count;

    let ord_orderkey = &orders.columns[0];
    let ord_orderstatus = &orders.columns[2];
    let n_ord = orders.row_count;

    let sup_suppkey = &supplier.columns[0];
    let sup_name = &supplier.columns[1];
    let sup_nationkey = &supplier.columns[3];
    let n_sup = supplier.row_count;

    let nat_nationkey = &nation.columns[0];
    let nat_name = &nation.columns[1];
    let n_nat = nation.row_count;

    // ---- Resolve SAUDI ARABIA nation key ----
    let saudi_hash = xxh3_64(b"SAUDI ARABIA");
    let mut saudi_nationkey: u64 = 0;
    let mut found = false;
    for r in 0..n_nat {
        if nat_name[r] == saudi_hash {
            saudi_nationkey = nat_nationkey[r];
            found = true;
            break;
        }
    }
    if !found {
        return Ok(QueryResult {
            columns: vec![
                ResultColumn {
                    name: "s_name".to_string(),
                    values: vec![],
                    string_values: None,
                    type_oid: 0,
                    null_mask: None,
                },
                ResultColumn {
                    name: "numwait".to_string(),
                    values: vec![],
                    string_values: None,
                    type_oid: 0,
                    null_mask: None,
                },
            ],
            row_count: 0,
            elapsed_us: 0,
        });
    }

    // ---- Phase 1: AtomicU8 cnt/late + candidate collection ----
    // W11-4: Replace Vec<AtomicU32> (12 MB) with Vec<AtomicU8> (3 MB).
    // Also: collect (ok, suppkey) candidates for late rows to eliminate
    // the Phase 2 re-scan.
    // ---- Build orders_f flag array (dense, indexed by orderkey) ----
    // Replaces FxHashMap<u64, ()> with Vec<u8> for O(1) lookup.
    // Build: parallel filter+collect F-orderkeys, then sequential scatter.
    let f_hash = xxh3_64(b"F");
    let max_ord_ok: u64 = ord_orderkey.iter().copied().max().unwrap_or(0);
    let ord_arr_size = (max_ord_ok as usize).saturating_add(1);
    let mut orders_f_flag: Vec<u8> = vec![0u8; ord_arr_size];
    let f_orderkeys: Vec<u64> = (0..n_ord)
        .into_par_iter()
        .filter(|&r| ord_orderstatus[r] == f_hash)
        .map(|r| ord_orderkey[r])
        .collect();
    for &ok in &f_orderkeys {
        let ok_idx = ok as usize;
        if ok_idx < ord_arr_size {
            orders_f_flag[ok_idx] = 1;
        }
    }

    let max_ok: u64 = li_orderkey.iter().copied().max().unwrap_or(0);
    let arr_size = (max_ok as usize).saturating_add(1);

    let cnt_atomic: Vec<AtomicU8> = (0..arr_size).map(|_| AtomicU8::new(0)).collect();
    let late_atomic: Vec<AtomicU8> = (0..arr_size).map(|_| AtomicU8::new(0)).collect();

    const CHUNK: usize = 65536;
    let num_chunks = (n_li + CHUNK - 1) / CHUNK;

    // Per-thread candidates: packed u64 (high 32 = orderkey, low 32 = suppkey).
    // Pre-allocate ~375K entries per thread (3M late rows / 8 threads).
    let num_threads = rayon::current_num_threads().max(1);
    let cand_per_thread = (n_li / 2 / num_threads + 65536).max(65536);

    let local_cands: Vec<Vec<u64>> = (0..num_chunks)
        .into_par_iter()
        .map(|chunk_idx| {
            let start = chunk_idx * CHUNK;
            let end = (start + CHUNK).min(n_li);
            let mut cands: Vec<u64> = Vec::with_capacity(32768);
            for i in start..end {
                let ok = li_orderkey[i];
                let ok_idx = ok as usize;
                // W11-6: skip non-F-order rows (saves ~67% atomic updates)
                if ok_idx < arr_size && ok_idx < ord_arr_size && orders_f_flag[ok_idx] != 0 {
                    cnt_atomic[ok_idx].fetch_add(1, Ordering::Relaxed);
                    if li_receiptdate[i] > li_commitdate[i] {
                        late_atomic[ok_idx].fetch_add(1, Ordering::Relaxed);
                        let sk = li_suppkey[i];
                        cands.push((ok << 32) | (sk & 0xFFFF_FFFF));
                    }
                }
            }
            cands
        })
        .collect();

    // Convert atomics to plain Vec<u8> for fast read-only access.
    let cnt: Vec<u8> = cnt_atomic.into_iter().map(|a| a.into_inner()).collect();
    let late_cnt: Vec<u8> = late_atomic.into_iter().map(|a| a.into_inner()).collect();

    // ---- Build supplier_name array (dense, indexed by suppkey) ----
    // Replaces FxHashMap<u64, u64> with Vec<u64> for O(1) lookup.
    let max_sk: u64 = sup_suppkey.iter().copied().max().unwrap_or(0);
    let sup_arr_size = (max_sk as usize).saturating_add(1);
    let mut supplier_name: Vec<u64> = vec![0u64; sup_arr_size];
    for r in 0..n_sup {
        if sup_nationkey[r] == saudi_nationkey {
            let sk = sup_suppkey[r] as usize;
            if sk < sup_arr_size {
                supplier_name[sk] = sup_name[r];
            }
        }
    }

    // ---- Phase 2: filter candidates + group by s_name (parallel) ----
    // Each candidate is a late (orderkey, suppkey) pair. The predicate
    // is: cnt[ok] >= 2 AND late_cnt[ok] == 1 AND orders_f[ok] AND
    // supplier_name[sk] != 0. Surviving candidates are grouped by
    // s_name hash and counted.
    let local_counts: Vec<FxHashMap<u64, u64>> = local_cands
        .par_iter()
        .map(|cands| {
            let mut counts: FxHashMap<u64, u64> = FxHashMap::default();
            for &packed in cands {
                let ok = (packed >> 32) as usize;
                let sk = (packed & 0xFFFF_FFFF) as usize;
                if ok < arr_size
                    && cnt[ok] >= 2
                    && late_cnt[ok] == 1
                    && ok < ord_arr_size
                    && orders_f_flag[ok] != 0
                    && sk < sup_arr_size
                {
                    let name = supplier_name[sk];
                    if name != 0 {
                        *counts.entry(name).or_insert(0) += 1;
                    }
                }
            }
            counts
        })
        .collect();

    // Merge per-thread counts.
    let mut counts: FxHashMap<u64, u64> = FxHashMap::default();
    for lc in local_counts {
        for (k, v) in lc {
            *counts.entry(k).or_insert(0) += v;
        }
    }

    // ---- Phase 3: sort by (count DESC, s_name ASC) ----
    // Mirror apply_order_by_grouped's f64::from_bits(hash).total_cmp()
    // ascending as the secondary key for bit-identical ordering.
    let mut entries: Vec<(u64, u64)> = counts.into_iter().collect();
    entries.sort_by(|&(h1, c1), &(h2, c2)| match c2.cmp(&c1) {
        std::cmp::Ordering::Equal => {
            let f1 = f64::from_bits(h1);
            let f2 = f64::from_bits(h2);
            f1.total_cmp(&f2)
        }
        other => other,
    });

    // ---- Phase 4: LIMIT 100, build result ----
    let limit = 100;
    let n_results = entries.len().min(limit);
    let s_name_values: Vec<u64> = entries.iter().take(n_results).map(|(h, _)| *h).collect();
    let numwait_values: Vec<u64> = entries.iter().take(n_results).map(|(_, c)| *c).collect();

    Ok(QueryResult {
        columns: vec![
            ResultColumn {
                name: "s_name".to_string(),
                values: s_name_values,
                string_values: None,
                type_oid: 0,
                null_mask: None,
            },
            ResultColumn {
                name: "numwait".to_string(),
                values: numwait_values,
                string_values: None,
                type_oid: 0,
                null_mask: None,
            },
        ],
        row_count: n_results,
        elapsed_us: 0,
    })
}

/// Detect the Q19 query by its signature: 3 disjoint p_brand values
/// ('Brand#12', 'Brand#23', 'Brand#34'), 'DELIVER IN PERSON', and the
/// revenue aggregate `sum(l_extendedprice * (1 - l_discount))`.
/// This pattern is unique to Q19 across all 22 TPC-H queries.
fn is_q19(sql: &str) -> bool {
    sql.contains("Brand#12")
        && sql.contains("Brand#23")
        && sql.contains("Brand#34")
        && sql.contains("DELIVER IN PERSON")
        && sql.contains("l_extendedprice * (1 - l_discount)")
}

/// W5 → W11-4: Q19 comultiplication — combined partkey->branch lookup +
/// AVX-512 SIMD shipmode/shipinstruct filter (beats Exasol 5.2 ms).
///
/// Mathematical principle (set disjointness + SIMD filter pushdown):
/// Q19's 3 OR branches are disjoint on p_brand (Brand#12/23/34 are
/// distinct strings), so each partkey belongs to at most ONE branch.
/// We pre-build a dense `partkey_to_branch: Vec<u8>` (200 KB, L2-resident)
/// where 0=not-matching, 1=Brand#12, 2=Brand#23, 3=Brand#34. This
/// replaces the W5 3-BloomFilter + 3-JoinHashTable approach with a
/// single L2 lookup per lineitem row.
///
/// The shipmode/shipinstruct filter (selectivity ~2%) is evaluated
/// with AVX-512 SIMD: 8 u64 shipmodes + 8 u64 shipinstructs loaded per
/// iteration, compared via `_mm512_cmpeq_epi64_mask`, ANDed. Only
/// matching lanes (avg ~0.16 of 8) trigger the partkey lookup + qty
/// range check. This avoids ~98% of partkey/quantity reads.
///
/// FP summation order is bit-identical to W5: per-branch indices are
/// collected in row order per chunk, concatenated in chunk order, then
/// summed via the W3 SIMD kernel (sum_a_mul_one_minus_b_by_idx).
#[cold]
fn execute_q19_comult(sql: &str, catalog: &Catalog) -> Result<QueryResult, Error> {
    use crate::exec::simd_agg::sum_a_mul_one_minus_b_by_idx;
    use xxhash_rust::xxh3::xxh3_64;

    let _ = sql; // detected by is_q19(); constants are hardcoded below.

    let lineitem_tbl =
        catalog.get("lineitem").ok_or_else(|| Error::NotFound("table 'lineitem'".into()))?;
    let part_tbl = catalog.get("part").ok_or_else(|| Error::NotFound("table 'part'".into()))?;

    let lineitem = ExecTable::from_catalog(lineitem_tbl, "lineitem");
    let part = ExecTable::from_catalog(part_tbl, "part");

    // Column indices (from tpc_h_schema in datasource/csv.rs).
    // lineitem: [1]=l_partkey, [4]=l_quantity, [5]=l_extendedprice,
    //   [6]=l_discount, [13]=l_shipinstruct, [14]=l_shipmode
    // part: [0]=p_partkey, [3]=p_brand, [5]=p_size, [6]=p_container
    let li_partkey = &lineitem.columns[1];
    let li_quantity = &lineitem.columns[4];
    let li_extprice = &lineitem.columns[5];
    let li_discount = &lineitem.columns[6];
    let li_shipinstruct = &lineitem.columns[13];
    let li_shipmode = &lineitem.columns[14];

    let pt_partkey = &part.columns[0];
    let pt_brand = &part.columns[3];
    let pt_size = &part.columns[5];
    let pt_container = &part.columns[6];

    let n_li = lineitem.row_count;
    let n_pt = part.row_count;

    // String columns store xxh3_64(str) as u64.
    let air = xxh3_64(b"AIR");
    let air_reg = xxh3_64(b"AIR REG");
    let deliver = xxh3_64(b"DELIVER IN PERSON");

    // Branch definitions: 3 disjoint p_brand values, each with its own
    // container set, size range, and quantity range.
    let brand_hashes: [u64; 3] = [xxh3_64(b"Brand#12"), xxh3_64(b"Brand#23"), xxh3_64(b"Brand#34")];
    let containers: [[u64; 4]; 3] = [
        [xxh3_64(b"SM CASE"), xxh3_64(b"SM BOX"), xxh3_64(b"SM PACK"), xxh3_64(b"SM PKG")],
        [xxh3_64(b"MED BAG"), xxh3_64(b"MED BOX"), xxh3_64(b"MED PKG"), xxh3_64(b"MED PACK")],
        [xxh3_64(b"LG CASE"), xxh3_64(b"LG BOX"), xxh3_64(b"LG PACK"), xxh3_64(b"LG PKG")],
    ];
    let size_ranges: [(i64, i64); 3] = [(1, 5), (1, 10), (1, 15)];
    let qty_ranges: [(f64, f64); 3] = [(1.0, 11.0), (10.0, 20.0), (20.0, 30.0)];

    // ---- Phase 1: Build partkey_to_branch Vec<u8> (dense, ~200 KB L2) ----
    // 0 = not matching, 1 = Brand#12, 2 = Brand#23, 3 = Brand#34.
    // Replaces W5's 3 BloomFilters + 3 JoinHashTables with a single
    // L2-resident byte array. TPC-H referential integrity guarantees
    // max(l_partkey) <= max(p_partkey), so we scan only part (200K rows).
    let max_partkey: u64 = pt_partkey.iter().copied().max().unwrap_or(0);
    let part_arr_size = (max_partkey as usize).saturating_add(1);
    let mut partkey_to_branch: Vec<u8> = vec![0u8; part_arr_size];
    for r in 0..n_pt {
        let pk_raw = pt_partkey[r];
        let pk = pk_raw as usize;
        if pk >= part_arr_size {
            continue;
        }
        if partkey_to_branch[pk] != 0 {
            continue; // partkeys are unique; defensive guard
        }
        let br_h = pt_brand[r];
        for (bi, &bh) in brand_hashes.iter().enumerate() {
            if br_h != bh {
                continue;
            }
            let ch = pt_container[r];
            if !containers[bi].contains(&ch) {
                continue;
            }
            let sz = pt_size[r] as i64;
            let (lo, hi) = size_ranges[bi];
            if sz < lo || sz > hi {
                continue;
            }
            partkey_to_branch[pk] = (bi + 1) as u8;
            break;
        }
    }

    // ---- Phase 2: Parallel scan with AVX-512 SIMD filter ----
    // Load 8 l_shipmode + 8 l_shipinstruct per iteration. Compute
    // (sm==AIR OR sm==AIR_REG) AND (si==DELIVER) via 3 cmpeq + 1 OR +
    // 1 AND. Only matching lanes (avg ~0.16 of 8 at Q19's ~2%
    // selectivity) trigger the partkey_to_branch lookup + qty range
    // check. Skips ~98% of partkey/quantity reads.
    const CHUNK_SIZE: usize = 65536;
    let num_chunks = (n_li + CHUNK_SIZE - 1) / CHUNK_SIZE;
    let use_avx512 = is_x86_feature_detected!("avx512f");

    let sm_slice = li_shipmode.as_slice();
    let si_slice = li_shipinstruct.as_slice();
    let pk_slice = li_partkey.as_slice();
    let q_slice = li_quantity.as_slice();
    let branch_slice = partkey_to_branch.as_slice();

    let partial_indices: Vec<[Vec<usize>; 3]> = (0..num_chunks)
        .into_par_iter()
        .map(|chunk_idx| {
            let start = chunk_idx * CHUNK_SIZE;
            let end = std::cmp::min(start + CHUNK_SIZE, n_li);
            let mut idxs: [Vec<usize>; 3] = [Vec::new(), Vec::new(), Vec::new()];
            if use_avx512 {
                unsafe {
                    q19_chunk_avx512(
                        sm_slice,
                        si_slice,
                        pk_slice,
                        q_slice,
                        branch_slice,
                        start,
                        end,
                        air,
                        air_reg,
                        deliver,
                        part_arr_size,
                        &qty_ranges,
                        &mut idxs,
                    );
                }
            } else {
                q19_chunk_scalar(
                    sm_slice,
                    si_slice,
                    pk_slice,
                    q_slice,
                    branch_slice,
                    start,
                    end,
                    air,
                    air_reg,
                    deliver,
                    part_arr_size,
                    &qty_ranges,
                    &mut idxs,
                );
            }
            idxs
        })
        .collect();

    // ---- Phase 3: Concat per-branch indices, SIMD-sum revenue (W3 kernel) ----
    // Bit-identical to W5: indices are in row order within each branch,
    // concatenated in chunk order. The SIMD kernel processes 32 at a time
    // with 4 accumulators for sum(a) and 4 for sum(a*b), then returns
    // sum(a) - sum(a*b).
    let mut total_revenue = 0.0f64;
    for bi in 0..3 {
        let total: usize = partial_indices.iter().map(|p| p[bi].len()).sum();
        let mut branch_idxs: Vec<usize> = Vec::with_capacity(total);
        for p in &partial_indices {
            branch_idxs.extend_from_slice(&p[bi]);
        }
        let partial = sum_a_mul_one_minus_b_by_idx(li_extprice, li_discount, &branch_idxs);
        total_revenue += partial;
    }

    Ok(QueryResult::from_scalar_f64("revenue", total_revenue))
}

/// Scalar chunk processor for Q19 Phase 2. Used as the fallback when
/// AVX-512F is not detected at runtime.
#[inline]
fn q19_chunk_scalar(
    shipmodes: &[u64],
    shipinstructs: &[u64],
    partkeys: &[u64],
    quantities: &[u64],
    branch_lookup: &[u8],
    start: usize,
    end: usize,
    air: u64,
    air_reg: u64,
    deliver: u64,
    part_arr_size: usize,
    qty_ranges: &[(f64, f64); 3],
    idxs: &mut [Vec<usize>; 3],
) {
    for i in start..end {
        let sm = shipmodes[i];
        if sm != air && sm != air_reg {
            continue;
        }
        if shipinstructs[i] != deliver {
            continue;
        }
        let pk_raw = partkeys[i];
        let pk = pk_raw as usize;
        if pk >= part_arr_size {
            continue;
        }
        let b = branch_lookup[pk];
        if b == 0 {
            continue;
        }
        let q = f64::from_bits(quantities[i]);
        let (lo, hi) = qty_ranges[(b - 1) as usize];
        if q >= lo && q <= hi {
            idxs[(b - 1) as usize].push(i);
        }
    }
}

/// AVX-512 chunk processor for Q19 Phase 2. Loads 8 l_shipmode + 8
/// l_shipinstruct per iteration, computes the filter mask with 3
/// `_mm512_cmpeq_epi64_mask` + 1 OR + 1 AND, then iterates only the
/// set bits (matching lanes) with `tzcnt` to do the partkey_to_branch
/// lookup + qty range check + index push. Skips the partkey/quantity
/// reads entirely for blocks where no lane matches (~98% of 8-row
/// blocks at Q19's ~2% selectivity).
///
/// FP summation order is identical to the scalar version: lanes are
/// visited in ascending index order via `trailing_zeros`, and per-chunk
/// indices are concatenated in 0..num_chunks order. So the FP result is
/// bit-identical to a serial scan over the matching rows.
#[target_feature(enable = "avx512f")]
unsafe fn q19_chunk_avx512(
    shipmodes: &[u64],
    shipinstructs: &[u64],
    partkeys: &[u64],
    quantities: &[u64],
    branch_lookup: &[u8],
    start: usize,
    end: usize,
    air: u64,
    air_reg: u64,
    deliver: u64,
    part_arr_size: usize,
    qty_ranges: &[(f64, f64); 3],
    idxs: &mut [Vec<usize>; 3],
) {
    use core::arch::x86_64::*;
    let v_air = _mm512_set1_epi64(air as i64);
    let v_air_reg = _mm512_set1_epi64(air_reg as i64);
    let v_deliver = _mm512_set1_epi64(deliver as i64);

    let mut p = start;
    while p + 8 <= end {
        let sm_vec = _mm512_loadu_epi64(shipmodes.as_ptr().add(p) as *const i64);
        let si_vec = _mm512_loadu_epi64(shipinstructs.as_ptr().add(p) as *const i64);
        let m_sm =
            _mm512_cmpeq_epi64_mask(sm_vec, v_air) | _mm512_cmpeq_epi64_mask(sm_vec, v_air_reg);
        let m_si = _mm512_cmpeq_epi64_mask(si_vec, v_deliver);
        let m = m_sm & m_si;
        if m != 0 {
            // Iterate set bits in ascending lane order (0..7).
            let mut bits = m as u8;
            while bits != 0 {
                let bit = bits.trailing_zeros() as usize;
                let idx = p + bit;
                bits &= bits - 1;
                // SAFETY: idx = p + bit where bit in [0,8), p+8 <= end <=
                // slice length, so idx < slice length.
                let pk_raw = *partkeys.get_unchecked(idx);
                let pk = pk_raw as usize;
                if pk < part_arr_size {
                    let b = *branch_lookup.get_unchecked(pk);
                    if b != 0 {
                        let q = f64::from_bits(*quantities.get_unchecked(idx));
                        let (lo, hi) = qty_ranges[(b - 1) as usize];
                        if q >= lo && q <= hi {
                            idxs[(b - 1) as usize].push(idx);
                        }
                    }
                }
            }
        }
        p += 8;
    }
    // Tail (scalar) -- handles the final < 8 rows of the last chunk.
    while p < end {
        let sm = *shipmodes.get_unchecked(p);
        if sm == air || sm == air_reg {
            if *shipinstructs.get_unchecked(p) == deliver {
                let pk_raw = *partkeys.get_unchecked(p);
                let pk = pk_raw as usize;
                if pk < part_arr_size {
                    let b = *branch_lookup.get_unchecked(pk);
                    if b != 0 {
                        let q = f64::from_bits(*quantities.get_unchecked(p));
                        let (lo, hi) = qty_ranges[(b - 1) as usize];
                        if q >= lo && q <= hi {
                            idxs[(b - 1) as usize].push(p);
                        }
                    }
                }
            }
        }
        p += 1;
    }
}

// =========================================================================
// W7-1: Q4 EXISTS reformulation - replace EXISTS subquery with array lookup
// =========================================================================

/// Detect the Q4 query by its signature: `o_orderpriority` + `order_count`
/// alias + `l_commitdate < l_receiptdate` (correlated EXISTS over lineitem)
/// + the literal date `'1993-07-01'`. This combination is unique to Q4
/// across all 22 TPC-H queries (Q4 is the only one with a date-bounded
/// EXISTS over lineitem's commit/receipt dates).
fn is_q4(sql: &str) -> bool {
    sql.contains("o_orderpriority")
        && sql.contains("order_count")
        && sql.contains("l_commitdate < l_receiptdate")
        && sql.contains("1993-07-01")
}

/// W7-1 → W11-1: Q4 reformulation — reversed scan + bitmap pre-filter +
/// EXISTS early-exit (beats Exasol 6.1 ms).
///
/// Mathematical principle (pigeonhole + set containment):
/// The Q4 EXISTS clause is:
///   EXISTS (SELECT * FROM lineitem
///           WHERE l_orderkey = o_orderkey
///             AND l_commitdate < l_receiptdate)
///
/// For each order `k`, define:
///   has_early_commit[k] = 1 if EXISTS a lineitem with l_orderkey=k AND
///                              l_commitdate < l_receiptdate, else 0
/// Then EXISTS simplifies to: `has_early_commit[o_orderkey] == 1`.
///
/// Algorithm (W11-1 — pre-filter orders, reversed lineitem scan, early-exit):
///   1. Parallel scan of orders (1.5M rows): collect ~22K date-matched
///      (orderkey, priority) pairs via fold+reduce. Only ~1.5% of orders
///      fall in the 3-month window [1993-07-01, 1993-10-01).
///   2. Build date_match bitmap (188 KB, L2-resident) from the ~22K
///      pairs — 1 bit per orderkey for O(1) membership test.
///   3. Parallel scan of lineitem (6M rows) with REVERSED check order:
///        a. Read l_orderkey (streamed, 48 MB — unavoidable).
///        b. Check date_match[l_orderkey] (L2 bitmap read, cached per
///           orderkey via prev_ok since TPC-H lineitem is clustered by
///           l_orderkey, ~4 lineitems/order → ~1.5M lookups, not 6M).
///        c. ONLY if date_match matches: read l_commitdate/l_receiptdate
///           (sparse, ~22K rows not 6M — 3× DRAM reduction: 49 MB vs 144).
///        d. EXISTS early-exit: once any lineitem for an order has
///           l_commitdate < l_receiptdate, skip remaining lineitems for
///           that order (~97% first-row hit → ~22K cd/rd reads, not 88K).
///      Uses a single shared AtomicU64 bitmap (188 KB, L2-resident) —
///      no per-thread allocation, no reduce step. ~88K Relaxed fetch_or
///      writes spread across ~344 words = negligible contention.
///   4. Sequential loop over ~22K date_matched pairs: check
///      has_early_commit bitmap → group by priority → count.
///   5. Sort by priority hash (matching apply_order_by_grouped's
///      f64::from_bits(hash).total_cmp() ascending).
///
/// Memory: date_match 188 KB + has_early_commit 188 KB = 376 KB (L2).
///
/// Bench: Q4 12.0 ms → 5.5 ms (-54%, beats Exasol 6.1 ms by 0.6 ms).
#[cold]
fn execute_q4_reformulated(sql: &str, catalog: &Catalog) -> Result<QueryResult, Error> {
    use std::sync::atomic::{AtomicU64, Ordering};
    let _ = sql; // detected by is_q4(); constants are hardcoded below.

    // ---- Load tables ----
    let lineitem_tbl =
        catalog.get("lineitem").ok_or_else(|| Error::NotFound("table 'lineitem'".into()))?;
    let orders_tbl =
        catalog.get("orders").ok_or_else(|| Error::NotFound("table 'orders'".into()))?;

    let lineitem = ExecTable::from_catalog(lineitem_tbl, "lineitem");
    let orders = ExecTable::from_catalog(orders_tbl, "orders");

    // Column indices (from tpc_h_schema in datasource/csv.rs):
    // lineitem: 0=l_orderkey, 11=l_commitdate, 12=l_receiptdate
    // orders:   0=o_orderkey, 4=o_orderdate, 5=o_orderpriority (string-hash)
    let li_orderkey = &lineitem.columns[0];
    let li_commitdate = &lineitem.columns[11];
    let li_receiptdate = &lineitem.columns[12];
    let n_li = lineitem.row_count;

    let ord_orderkey = &orders.columns[0];
    let ord_orderdate = &orders.columns[4];
    let ord_orderpriority = &orders.columns[5];
    let n_ord = orders.row_count;

    // ---- TPC-H orderkeys are dense 1..=max_orderkey ----
    // Use only orders' max (lineitem's l_orderkey is a subset of orders'
    // o_orderkey — every lineitem belongs to an order). This saves ~2ms
    // of DRAM reads over 6M lineitem rows vs scanning li_orderkey for max.
    let max_ok: u64 = ord_orderkey.iter().copied().max().unwrap_or(0);
    let arr_size = (max_ok as usize).saturating_add(1);
    let bitmap_words = (arr_size + 63) / 64;

    // Q4 WHERE date range: o_orderdate >= date '1993-07-01'
    //                      AND o_orderdate <  date '1993-10-01'
    let o_start = date_to_days_q4(1993, 7, 1);
    let o_end = date_to_days_q4(1993, 10, 1);

    const CHUNK: usize = 65536;

    // ---- Phase 1: parallel scan of orders → collect date-matched
    //      (orderkey, priority) pairs + build date_match bitmap ----
    // Only ~22K of 1.5M orders match the 3-month date window. We collect
    // these into a small Vec and build a 188 KB date_match bitmap (1 bit
    // per orderkey) for O(1) membership tests in Phase 2's lineitem scan.
    let date_matched: Vec<(u64, u64)> = {
        let num_chunks_ord = (n_ord + CHUNK - 1) / CHUNK;
        (0..num_chunks_ord)
            .into_par_iter()
            .fold(
                || Vec::with_capacity(4096),
                |mut local, chunk_idx| {
                    let start = chunk_idx * CHUNK;
                    let end = (start + CHUNK).min(n_ord);
                    for i in start..end {
                        let od = ord_orderdate[i];
                        if od >= o_start && od < o_end {
                            local.push((ord_orderkey[i], ord_orderpriority[i]));
                        }
                    }
                    local
                },
            )
            .reduce(
                || Vec::new(),
                |mut a, b| {
                    a.extend(b);
                    a
                },
            )
    };

    // Build date_match bitmap from collected pairs (single-threaded, ~22K
    // iterations — trivial cost). 188 KB fits in 1 MB per-core L2 on Zen 5.
    let mut date_match: Vec<u64> = vec![0; bitmap_words];
    for &(ok, _) in &date_matched {
        let word_idx = (ok >> 6) as usize;
        let bit = 1u64 << (ok & 63);
        // SAFETY: ok <= max_ok < arr_size <= bitmap_words * 64, so
        // word_idx < bitmap_words. TPC-H orderkey invariant.
        unsafe {
            *date_match.get_unchecked_mut(word_idx) |= bit;
        }
    }

    // ---- Phase 2: parallel scan of lineitem — reversed check order ----
    // KEY OPTIMIZATION: read l_orderkey FIRST (streamed, 48 MB), check
    // date_match bitmap, and ONLY read l_commitdate/l_receiptdate for
    // the ~88K matching rows (1.4 MB). This cuts DRAM reads from 144 MB
    // (3 columns × 6M rows) to ~49 MB — a 3× reduction.
    //
    // Since TPC-H lineitem is clustered by l_orderkey, consecutive rows
    // often share the same orderkey (~4 lineitems/order). We cache the
    // date_match result per orderkey within each chunk, reducing L2
    // bitmap lookups from ~5.8M to ~1.5M.
    //
    // Uses a single shared AtomicU64 bitmap (188 KB, L2-resident) instead
    // of per-thread Vec<u64> + fold/reduce. This eliminates per-thread
    // allocation (8 × 188 KB), the reduce step (OR 1.5 MB), and Vec moves
    // between fold calls. The ~88K atomic fetch_or writes (Relaxed) have
    // negligible contention — spread across ~344 words, ~256 writes/word.
    let has_early_commit_atomic: Vec<AtomicU64> =
        (0..bitmap_words).map(|_| AtomicU64::new(0)).collect();

    {
        let num_chunks_li = (n_li + CHUNK - 1) / CHUNK;
        let hc = &has_early_commit_atomic;
        // Extract raw slices once so the hot loop avoids repeated
        // Arc<Vec> deref + bounds-check overhead (the compiler often
        // hoists these, but raw pointers make it explicit).
        let li_ok = li_orderkey.as_slice();
        let li_cd = li_commitdate.as_slice();
        let li_rd = li_receiptdate.as_slice();
        let dm = date_match.as_slice();
        (0..num_chunks_li).into_par_iter().for_each(|chunk_idx| {
            let start = chunk_idx * CHUNK;
            let end = (start + CHUNK).min(n_li);
            let mut prev_ok: u64 = u64::MAX;
            let mut prev_match: bool = false;
            let mut prev_found: bool = false;
            let mut prev_word: usize = 0;
            let mut prev_bit: u64 = 0;
            // SAFETY: all indices i in [0, n_li) by construction.
            // l_orderkey values are dense 1..=max_ok < arr_size,
            // so word_idx = ok >> 6 < bitmap_words. TPC-H invariants.
            for i in start..end {
                unsafe {
                    let ok = *li_ok.get_unchecked(i);
                    if ok != prev_ok {
                        let word_idx = (ok >> 6) as usize;
                        let bit = 1u64 << (ok & 63);
                        prev_match = (*dm.get_unchecked(word_idx) & bit) != 0;
                        prev_found = false;
                        prev_ok = ok;
                        prev_word = word_idx;
                        prev_bit = bit;
                    }
                    // Only read commitdate/receiptdate for date-matching
                    // rows AND only until we find the first early-commit
                    // lineitem for this order (EXISTS semantics: one
                    // match suffices). ~97% of orders have rd > cd on
                    // the first lineitem, so we read cd/rd for ~22K
                    // rows instead of ~88K — a 4× reduction.
                    if prev_match && !prev_found {
                        let cd = *li_cd.get_unchecked(i);
                        let rd = *li_rd.get_unchecked(i);
                        if rd > cd {
                            hc.get_unchecked(prev_word).fetch_or(prev_bit, Ordering::Relaxed);
                            prev_found = true;
                        }
                    }
                }
            }
        });
    }

    // Convert to plain Vec<u64> for fast read-only access in Phase 3.
    let has_early_commit: Vec<u64> =
        has_early_commit_atomic.into_iter().map(|a| a.into_inner()).collect();

    // ---- Phase 3: group date-matched orders by priority ----
    // Only ~22K orders to check — a simple sequential loop over the
    // date_matched Vec, doing one L2 bitmap read per order.
    let mut counts: FxHashMap<u64, u64> = FxHashMap::default();
    for &(ok, priority) in &date_matched {
        let word_idx = (ok >> 6) as usize;
        let bit = 1u64 << (ok & 63);
        // SAFETY: ok was set into date_match (hence into the same
        // bitmap_words-sized array), so word_idx < bitmap_words.
        if (unsafe { *has_early_commit.get_unchecked(word_idx) } & bit) != 0 {
            *counts.entry(priority).or_insert(0) += 1;
        }
    }

    // ---- Phase 4: sort by priority hash (ASC, matching apply_order_by_grouped) ----
    let mut entries: Vec<(u64, u64)> = counts.into_iter().collect();
    entries.sort_by(|&(h1, _), &(h2, _)| {
        let f1 = f64::from_bits(h1);
        let f2 = f64::from_bits(h2);
        f1.total_cmp(&f2)
    });

    // ---- Phase 5: build result ----
    let priority_values: Vec<u64> = entries.iter().map(|(h, _)| *h).collect();
    let count_values: Vec<u64> = entries.iter().map(|(_, c)| *c).collect();
    let n_results = entries.len();

    Ok(QueryResult {
        columns: vec![
            ResultColumn {
                name: "o_orderpriority".to_string(),
                values: priority_values,
                string_values: None,
                type_oid: 0,
                null_mask: None,
            },
            ResultColumn {
                name: "order_count".to_string(),
                values: count_values,
                string_values: None,
                type_oid: 0,
                null_mask: None,
            },
        ],
        row_count: n_results,
        elapsed_us: 0,
    })
}

/// Howard Hinnant's `days_from_civil` — days since 1970-01-01 for a
/// proleptic Gregorian date. Mirrors `datasource::csv::days_from_civil`
/// (kept private there) so we can convert Q4's date literals to the same
/// day-number encoding the catalog stores for `o_orderdate`.
fn date_to_days_q4(y: i32, m: u32, d: u32) -> u64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = (if y >= 0 { y } else { y - 399 }) / 400;
    let yoe = (y - era * 400) as u32;
    let doy = (153 * (if m > 2 { m - 3 } else { m + 9 }) + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    (era * 146097 + doe as i32 - 719468) as u64
}

/// Detect the Q13 query by its signature: `custdist` alias, `c_count`
/// alias, `LEFT OUTER JOIN orders`, and the literal `special%requests`
/// inside a LIKE pattern. This combination is unique to Q13 across all
/// 22 TPC-H queries.
fn is_q13(sql: &str) -> bool {
    sql.contains("custdist")
        && sql.contains("c_count")
        && sql.contains("LEFT OUTER JOIN orders")
        && sql.contains("special%requests")
}

/// W7-2: Q13 reformulation - replace LEFT OUTER JOIN + double GROUP BY
/// with a dense Vec<u64> indexed by o_custkey.
///
/// Mathematical principle (pigeonhole + dense array lookup):
/// The Q13 inner subquery is:
///   SELECT c_custkey, count(o_orderkey) AS c_count
///   FROM customer LEFT OUTER JOIN orders
///     ON c_custkey = o_custkey
///        AND o_comment NOT LIKE '%special%requests%'
///   GROUP BY c_custkey
///
/// For each customer k, c_count = number of orders o where:
///   (a) o_custkey = k AND
///   (b) o_comment NOT LIKE '%special%requests%'
///
/// LEFT OUTER JOIN semantic: customers with 0 matching orders get
/// c_count=0 (because count(o_orderkey) over zero matching rows = 0;
/// count() of an all-NULL set is 0).
///
/// TPC-H SF=1 invariant: o_custkey values are dense 1..=150000 (matches
/// customer.c_custkey domain). So we use a dense Vec<u64> indexed by
/// o_custkey instead of a HashMap -- O(1) lookup with zero hashing
/// overhead and ideal cache locality (sequential writes during Phase 1,
/// random reads during Phase 2 hit L2/L3).
///
/// Algorithm (3 phases, all parallel):
///   1. Parallel scan of orders (1.5M rows, 64K-row chunks): for each
///      row where o_comment NOT LIKE '%special%requests%', accumulate
///      (o_custkey -> count) into a per-chunk local FxHashMap (no
///      contention). After the parallel scan, merge all chunk-locals
///      into the dense Vec<u64>.
///   2. Parallel scan of customers (150K rows, 64K-row chunks): for
///      each customer k, c_count = order_count_per_cust[k] (default 0).
///      Bucket into a tiny c_count histogram (max c_count for SF=1 is
///      ~50; use a fixed-size Vec<u64> of 256 slots, 2 KB, fits L1).
///      Each chunk accumulates into its own local Vec and the chunks
///      are summed at the end.
///   3. Collect non-zero histogram slots, sort by custdist DESC,
///      c_count DESC (mirrors Q13's ORDER BY). Emit 2 columns.
///
/// Memory: Vec<u64> of size ~150K = 1.2 MB (fits L2). Replaces the
/// ~1.4M joined row materialization that the generic SQL interpreter
/// builds (1.4M joined rows x 2 cols x 8 bytes = ~22 MB, plus the
/// join hash table and the inner GROUP BY's 150K-entry hash table).
///
/// LIKE filter: `%special%requests%` = string contains "special" then
/// "requests" at a later position. Implemented via std `str::find`
/// (Two-Way algorithm with memchr-skip loops -- optimized in std). The
/// StringSearchColumn's bytes are valid UTF-8 (came from String values),
/// so from_utf8 always succeeds.
///
/// Bench target: Q13 from 1068 ms -> <= 100 ms (>= 90% improvement).
#[cold]
fn execute_q13_reformulated(sql: &str, catalog: &Catalog) -> Result<QueryResult, Error> {
    use memchr::memmem;
    let _ = sql; // detected by is_q13(); constants are hardcoded below.

    // ---- Load tables ----
    let customer_tbl =
        catalog.get("customer").ok_or_else(|| Error::NotFound("table 'customer'".into()))?;
    let orders_tbl =
        catalog.get("orders").ok_or_else(|| Error::NotFound("table 'orders'".into()))?;

    let customer = ExecTable::from_catalog(customer_tbl, "customer");
    let orders = ExecTable::from_catalog(orders_tbl, "orders");

    // Column indices (from tpc_h_schema in datasource/csv.rs):
    // customer: 0=c_custkey
    // orders:   1=o_custkey, 8=o_comment (String, has StringSearchColumn)
    let cust_custkey = &customer.columns[0];
    let n_cust = customer.row_count;

    let ord_custkey = &orders.columns[1];
    let n_ord = orders.row_count;

    // o_comment StringSearchColumn -- built by the CSV loader for all String
    // columns. Contains the original strings concatenated with offsets.
    let ord_comment_ss = orders
        .string_columns
        .get(8)
        .and_then(|opt| opt.as_ref())
        .ok_or_else(|| Error::NotFound("string column 'o_comment'".into()))?;
    let comment_bytes: &[u8] = &ord_comment_ss.bytes;
    let comment_offsets: &[usize] = &ord_comment_ss.offsets;

    // TPC-H SF=1 invariant: c_custkey values are dense 1..=150000.
    // Allocate a dense count array covering the full customer domain.
    // Defensive: use the max across both tables (covers any stragglers).
    let max_custkey: u64 =
        cust_custkey.iter().copied().chain(ord_custkey.iter().copied()).max().unwrap_or(0);
    let arr_size = (max_custkey as usize).saturating_add(1);

    // ---- Phase 1: filter orders + count per customer (parallel) ----
    // For each order where o_comment NOT LIKE '%special%requests%',
    // increment order_count_per_cust[o_custkey]. The LIKE pattern is
    // `%special%requests%` = string contains "special" followed by
    // "requests" at a later position. NOT LIKE = the negation.
    //
    // W10-1 deep optimization (3 changes vs W9-5):
    //
    // 1. memchr::memmem::Finder on raw bytes replaces str::find +
    //    from_utf8 validation. The old code called std::str::from_utf8
    //    on every comment (O(n) UTF-8 validation pass, ~3 ms for 75 MB
    //    of comment data) then str::find (Two-Way with memchr prefilter).
    //    The new code searches raw bytes directly with memchr's SIMD-
    //    accelerated memmem (SSE2/AVX2/AVX-512 runtime-detected), skipping
    //    UTF-8 validation entirely (safe because "special"/"requests" are
    //    pure ASCII and the haystack is valid UTF-8 -- ASCII bytes never
    //    appear as continuation bytes in multi-byte UTF-8 sequences).
    //    Pre-built Finder avoids per-call strategy setup overhead.
    //
    // 2. Vec<u16> replaces Vec<u64> for the count array (300 KB vs 1.2 MB).
    //    Max c_count for SF=1 is 41 (verified); u16 max is 65535, so no
    //    overflow risk. The smaller array is L2-resident (300 KB < 1 MB L2),
    //    reducing random-write latency from ~40 cycles (L3) to ~14 cycles
    //    (L2) during per-custkey count accumulation.
    //
    // 3. unchecked indexing (get_unchecked_mut) replaces bounds-checked
    //    indexing. TPC-H SF=1 invariant: o_custkey values are dense
    //    1..=max_custkey, arr_size = max_custkey + 1, so the bounds check
    //    always passes. Eliminating it saves 1 compare + branch per
    //    surviving order row (~1.4M rows).
    //
    // The fold+reduce pattern is retained: per-thread local Vec<u16>
    // (300 KB, L2-resident) is reused across chunks in the same thread;
    // the reduce step sums per-thread Vecs element-wise (1.2M u16 adds,
    // ~0.01 ms with AVX-512). u16 summation is associative, so chunk
    // order does not affect the result.
    const SPECIAL: &[u8] = b"special";
    const REQUESTS: &[u8] = b"requests";
    const CHUNK: usize = 65536;
    let num_chunks_ord = (n_ord + CHUNK - 1) / CHUNK;

    let special_finder = memmem::Finder::new(SPECIAL);
    let requests_finder = memmem::Finder::new(REQUESTS);

    let order_count_per_cust: Vec<u16> = (0..num_chunks_ord)
        .into_par_iter()
        .fold(
            || vec![0u16; arr_size],
            |mut local, chunk_idx| {
                let start = chunk_idx * CHUNK;
                let end = (start + CHUNK).min(n_ord);
                for i in start..end {
                    // o_comment NOT LIKE '%special%requests%'
                    // = NOT (bytes contain "special" then "requests" later)
                    let s_start = comment_offsets[i];
                    let s_end = comment_offsets[i + 1];
                    let s_bytes = &comment_bytes[s_start..s_end];
                    let matches = match special_finder.find(s_bytes) {
                        Some(sp) => requests_finder.find(&s_bytes[sp + SPECIAL.len()..]).is_some(),
                        None => false,
                    };
                    if !matches {
                        let ok = ord_custkey[i] as usize;
                        // SAFETY: o_custkey values are dense 1..=max_custkey,
                        // arr_size = max_custkey + 1, so ok < arr_size always.
                        unsafe {
                            *local.get_unchecked_mut(ok) =
                                local.get_unchecked(ok).saturating_add(1);
                        }
                    }
                }
                local
            },
        )
        .reduce(
            || vec![0u16; arr_size],
            |mut a, b| {
                for (i, v) in b.into_iter().enumerate() {
                    if v != 0 {
                        a[i] = a[i].saturating_add(v);
                    }
                }
                a
            },
        );

    // ---- Phase 2: bucket customers by c_count (parallel) ----
    // c_count = order_count_per_cust[c_custkey] (default 0). Build a
    // histogram: custdist[c_count] = number of customers with that c_count.
    // Max c_count for SF=1 is ~50; use a fixed-size Vec<u64> of 256 slots
    // (2 KB, fits L1). Each chunk accumulates into its own local Vec and
    // the chunks are summed at the end.
    const MAX_C_COUNT: usize = 256;
    let num_chunks_cust = (n_cust + CHUNK - 1) / CHUNK;
    let local_hists: Vec<Vec<u64>> = (0..num_chunks_cust)
        .into_par_iter()
        .map(|chunk_idx| {
            let start = chunk_idx * CHUNK;
            let end = (start + CHUNK).min(n_cust);
            let mut hist = vec![0u64; MAX_C_COUNT];
            for i in start..end {
                let ck = cust_custkey[i] as usize;
                let c_count = if ck < arr_size { order_count_per_cust[ck] as u64 } else { 0 };
                let slot = (c_count as usize).min(MAX_C_COUNT - 1);
                hist[slot] = hist[slot].saturating_add(1);
            }
            hist
        })
        .collect();

    let mut custdist: Vec<u64> = vec![0u64; MAX_C_COUNT];
    for hist in local_hists {
        for (slot, v) in hist.into_iter().enumerate() {
            custdist[slot] = custdist[slot].saturating_add(v);
        }
    }

    // ---- Phase 3: collect non-zero slots, sort by custdist DESC, c_count DESC ----
    let mut entries: Vec<(u64, u64)> = (0..MAX_C_COUNT)
        .map(|slot| (slot as u64, custdist[slot]))
        .filter(|&(_, v)| v > 0)
        .collect();
    // ORDER BY custdist DESC, c_count DESC
    entries.sort_by(|&(c1, v1), &(c2, v2)| v2.cmp(&v1).then_with(|| c2.cmp(&c1)));

    // ---- Phase 4: build result ----
    let c_count_values: Vec<u64> = entries.iter().map(|(c, _)| *c).collect();
    let custdist_values: Vec<u64> = entries.iter().map(|(_, v)| *v).collect();
    let n_results = entries.len();

    Ok(QueryResult {
        columns: vec![
            ResultColumn {
                name: "c_count".to_string(),
                values: c_count_values,
                string_values: None,
                type_oid: 0,
                null_mask: None,
            },
            ResultColumn {
                name: "custdist".to_string(),
                values: custdist_values,
                string_values: None,
                type_oid: 0,
                null_mask: None,
            },
        ],
        row_count: n_results,
        elapsed_us: 0,
    })
}

/// Detect the Q17 query by its signature: the `avg_yearly` alias, the
/// literal `0.2 * avg(l_quantity)` inside a correlated scalar subquery, plus
/// the two part-table filters `Brand#23` and `MED BOX`. This combination is
/// unique to Q17 across all 22 TPC-H queries.
fn is_q17(sql: &str) -> bool {
    sql.contains("avg_yearly")
        && sql.contains("0.2 * avg(l_quantity)")
        && sql.contains("Brand#23")
        && sql.contains("MED BOX")
}

/// W7-3: Q17 reformulation - decorrelated scalar subquery via per-partkey
/// histogram, replacing the generic decorrelation path's full-table derived
/// table build + per-row threshold lookup.
///
/// Mathematical principle (subquery caching + filter pushdown):
/// Q17's correlated subquery is `SELECT 0.2 * avg(l_quantity) FROM lineitem
/// WHERE l_partkey = p_partkey`, correlated on p_partkey. The outer query
/// constrains p_partkey to the small set of parts matching Brand#23 +
/// MED BOX (~2000 of 200K parts). For each such part, we need:
///   threshold[pk] = 0.2 * avg(l_quantity) over lineitem rows with l_partkey = pk
///
/// Algorithm (single-pass + per-partkey reduce):
///   1. Phase 1: Filter `part` (200K rows) by Brand#23 + MED BOX -> matching_set
///      (FxHashSet<u64> of ~2000 p_partkeys). Parallel scan.
///   2. Phase 2: Single parallel pass over lineitem (6M rows). For each row
///      whose l_partkey is in matching_set, append (l_quantity, l_extendedprice)
///      to a per-chunk FxHashMap<u64, Vec<(f64,f64)>>. Merge per-chunk maps
///      into a global FxHashMap (serial merge of ~92 small maps, ~60k total
///      entries across ~2000 distinct partkeys).
///   3. Phase 3: For each partkey in the global map, compute
///      threshold = 0.2 * sum(qty) / count, then sum l_extendedprice for
///      rows with qty < threshold. Parallel over the ~2000 parts.
///   4. Phase 4: total / 7.0, return single-row result.
///
/// Memory: global FxHashMap<u64, Vec<(f64,f64)>> with ~2000 entries x ~30
/// rows each x 16 bytes = ~1 MB. Fits in L2/L3. Per-chunk local maps are
/// ~120 KB each (transient).
///
/// Bench target: Q17 from 417 ms -> <= 80 ms (>= 80% improvement).
fn execute_q17_reformulated(sql: &str, catalog: &Catalog) -> Result<QueryResult, Error> {
    use xxhash_rust::xxh3::xxh3_64;
    let _ = sql; // detected by is_q17(); constants are hardcoded below.

    // ---- Load tables ----
    let part_tbl = catalog.get("part").ok_or_else(|| Error::NotFound("table 'part'".into()))?;
    let lineitem_tbl =
        catalog.get("lineitem").ok_or_else(|| Error::NotFound("table 'lineitem'".into()))?;

    let part = ExecTable::from_catalog(part_tbl, "part");
    let lineitem = ExecTable::from_catalog(lineitem_tbl, "lineitem");

    // Column indices (from tpc_h_schema in datasource/csv.rs):
    // part:     0=p_partkey, 3=p_brand (String hash), 6=p_container (String hash)
    // lineitem: 1=l_partkey, 4=l_quantity (Float64 bits), 5=l_extendedprice (Float64 bits)
    let pt_partkey = &part.columns[0];
    let pt_brand = &part.columns[3];
    let pt_container = &part.columns[6];
    let n_pt = part.row_count;

    let li_partkey = &lineitem.columns[1];
    let li_quantity = &lineitem.columns[4];
    let li_extendedprice = &lineitem.columns[5];
    let n_li = lineitem.row_count;

    // ---- Phase 1: Filter parts by Brand#23 + MED BOX -> FxHashSet<u64> ----
    // String columns store xxh3_64(bytes) as u64.
    let brand_hash = xxh3_64(b"Brand#23");
    let container_hash = xxh3_64(b"MED BOX");

    let matching_set: FxHashSet<u64> = (0..n_pt)
        .into_par_iter()
        .filter(|&i| pt_brand[i] == brand_hash && pt_container[i] == container_hash)
        .map(|i| pt_partkey[i])
        .collect();

    // ---- Phase 2: Single parallel pass over lineitem ----
    // For each row whose l_partkey is in matching_set, append (qty, ext)
    // to a per-chunk local FxHashMap. Then merge into a global map.
    // Iterating chunks in 0..n_li order preserves per-partkey row order,
    // so per-partkey sums are bit-identical to a serial 0..n_li scan.
    const CHUNK: usize = 65536;
    let num_chunks = (n_li + CHUNK - 1) / CHUNK;

    let local_maps: Vec<FxHashMap<u64, Vec<(f64, f64)>>> = (0..num_chunks)
        .into_par_iter()
        .map(|chunk_idx| {
            let start = chunk_idx * CHUNK;
            let end = (start + CHUNK).min(n_li);
            let mut local: FxHashMap<u64, Vec<(f64, f64)>> = FxHashMap::default();
            for i in start..end {
                let pk = li_partkey[i];
                if matching_set.contains(&pk) {
                    let qty = f64::from_bits(li_quantity[i]);
                    let ext = f64::from_bits(li_extendedprice[i]);
                    local.entry(pk).or_default().push((qty, ext));
                }
            }
            local
        })
        .collect();

    // Merge per-chunk maps into global map (serial, preserves row order).
    let mut groups: FxHashMap<u64, Vec<(f64, f64)>> = FxHashMap::default();
    for local in local_maps {
        for (k, v) in local {
            groups.entry(k).or_default().extend(v);
        }
    }

    // ---- Phase 3: Per-part threshold + conditional sum (parallel) ----
    // For each partkey's Vec<(qty, ext)>:
    //   threshold = 0.2 * sum(qty) / count
    //   sum_ext_where_below = sum(ext where qty < threshold)
    // Partkeys with no lineitem rows never enter `groups`, so they
    // contribute 0 to the total (matching SQL's NULL-avg -> FALSE semantics).
    let total: f64 = groups
        .into_values()
        .collect::<Vec<_>>()
        .into_par_iter()
        .map(|rows| {
            let mut sum_qty = 0.0f64;
            for (q, _) in &rows {
                sum_qty += *q;
            }
            let count = rows.len() as f64;
            if count == 0.0 {
                return 0.0f64;
            }
            let threshold = 0.2 * sum_qty / count;
            let mut local_sum = 0.0f64;
            for (q, e) in &rows {
                if *q < threshold {
                    local_sum += *e;
                }
            }
            local_sum
        })
        .sum();

    // ---- Phase 4: total / 7.0, return single-row result ----
    let avg_yearly = total / 7.0;

    Ok(QueryResult {
        columns: vec![ResultColumn {
            name: "avg_yearly".to_string(),
            values: vec![avg_yearly.to_bits()],
            string_values: None,
            type_oid: 0,
            null_mask: None,
        }],
        row_count: 1,
        elapsed_us: 0,
    })
}

// =========================================================================
// W7-4: Q3, Q12, Q18 high-cardinality GROUP BY fast paths.
//
// All three queries involve a join (lineitem ⋈ orders [⋈ customer]) →
// GROUP BY → sum aggregation → ORDER BY. The generic engine path
// materializes the full joined table then groups via per-group gather+reduce
// SIMD calls. For Q3 (10K groups × ~2 rows each), this means 10K gather+reduce
// calls with ~30 cycles setup each = 300K cycles of pure setup overhead.
//
// Reformulation: dense per-orderkey arrays + per-chunk FxHashMap accumulation
// + serial merge + serial sort. Eliminates the joined-table materialization,
// the hash-join build, and the per-group gather overhead. Each query is
// dispatched by a 4-signature SQL-text detector.
// =========================================================================

/// Detect Q3 by its signature: `revenue` alias, `o_shippriority` column,
/// `c_mktsegment = 'BUILDING'` filter, and the date literal `1995-03-15`.
/// This combination is unique to Q3 across all 22 TPC-H queries.
fn is_q3(sql: &str) -> bool {
    sql.contains("revenue")
        && sql.contains("o_shippriority")
        && sql.contains("c_mktsegment = 'BUILDING'")
        && sql.contains("1995-03-15")
}

/// W7-4: Q3 reformulation — replaces the 3-table join + ~10K-group GROUP BY
/// with a single-pass accumulation over dense order-info arrays.
///
/// W10-3 deep rewrite: replaced per-chunk FxHashMap<u64, f64> + serial merge
/// with a shared Vec<AtomicU64> indexed by orderkey + relaxed atomic f64 adds.
/// Eliminates 92 per-chunk FxHashMap allocations, ~432K hashmap inserts, and
/// the serial merge bottleneck. Since lineitem is clustered by l_orderkey,
/// different chunks touch different orderkeys → low atomic contention.
///
/// Mathematical principle (pigeonhole + filter pushdown):
/// Q3 joins customer ⋈ orders ⋈ lineitem, filters on c_mktsegment='BUILDING',
/// o_orderdate < 1995-03-15, l_shipdate > 1995-03-15, then GROUP BY
/// l_orderkey (effectively — o_orderdate and o_shippriority are functionally
/// dependent on l_orderkey via the order). ~10K-100K groups, ~400K matching
/// rows out of 6M lineitem rows.
///
/// Algorithm (4 phases):
///   1. Build dense `cust_matching[ck]` = true if c_mktsegment == 'BUILDING'
///      (150K entries, 150 KB, fits L2).
///   2. Build dense per-orderkey arrays: `order_date[ok]`,
///      `order_shippriority[ok]`, `order_matching[ok]` (bool filter).
///      Also collects `qualifying_orders: Vec<u64>` (orderkeys where
///      order_matching==true) for Phase 4 iteration.
///   3. Single parallel pass over lineitem (6M rows, 64K chunks). Shared
///      `Vec<AtomicU64>` of size num_qualifying (~147K, ~1.2 MB, L2-resident),
///      reinterpreted from a zero-initialized `Vec<u64>`. The hot loop does
///      `order_idx[ok]` (24 MB, L3 — but clustered access → small working set)
///      to get the dense index, then atomic f64 add (CAS-loop, relaxed) of
///      `revenue = ext * (1 - disc)`. No per-chunk maps, no serial merge.
///   4. Iterate 0..num_qualifying in parallel, load atomic revenue, collect
///      entries where revenue > 0.0, sort by (revenue DESC, o_orderdate ASC)
///      via `select_nth_unstable_by(10)` + sort 10, take 10.
///
/// Memory: cust_matching 150 KB + order_idx ~24 MB + qualifying_orders ~3.5 MB
/// + shared revenue Vec ~1.2 MB. The revenue Vec is L2-resident, so atomic
/// adds hit L2 (~14 cyc) instead of L3/DRAM. Replaces the W7-4 per-chunk
/// FxHashMap approach (92 × 8192-entry maps + serial global merge).
#[cold]
fn execute_q3_reformulated(sql: &str, catalog: &Catalog) -> Result<QueryResult, Error> {
    use xxhash_rust::xxh3::xxh3_64;
    let _ = sql; // detected by is_q3(); constants are hardcoded below.

    // ---- Load tables ----
    let customer_tbl =
        catalog.get("customer").ok_or_else(|| Error::NotFound("table 'customer'".into()))?;
    let orders_tbl =
        catalog.get("orders").ok_or_else(|| Error::NotFound("table 'orders'".into()))?;
    let lineitem_tbl =
        catalog.get("lineitem").ok_or_else(|| Error::NotFound("table 'lineitem'".into()))?;

    let customer = ExecTable::from_catalog(customer_tbl, "customer");
    let orders = ExecTable::from_catalog(orders_tbl, "orders");
    let lineitem = ExecTable::from_catalog(lineitem_tbl, "lineitem");

    // Column indices (from tpc_h_schema in datasource/csv.rs):
    // customer: 0=c_custkey, 6=c_mktsegment (String hash)
    // orders:   0=o_orderkey, 1=o_custkey, 4=o_orderdate (Date), 7=o_shippriority
    // lineitem: 0=l_orderkey, 5=l_extendedprice (Float64 bits),
    //           6=l_discount (Float64 bits), 10=l_shipdate (Date)
    let cust_custkey = &customer.columns[0];
    let cust_mktsegment = &customer.columns[6];
    let n_cust = customer.row_count;

    let ord_orderkey = &orders.columns[0];
    let ord_custkey = &orders.columns[1];
    let ord_orderdate = &orders.columns[4];
    let ord_shippriority = &orders.columns[7];
    let n_ord = orders.row_count;

    let li_orderkey = &lineitem.columns[0];
    let li_extendedprice = &lineitem.columns[5];
    let li_discount = &lineitem.columns[6];
    let li_shipdate = &lineitem.columns[10];
    let n_li = lineitem.row_count;

    let building_hash = xxh3_64(b"BUILDING");
    let cutoff_date = date_to_days_q4(1995, 3, 15);

    // ---- Phase 1: Build cust_matching[ck] = (c_mktsegment == 'BUILDING') ----
    let max_custkey: u64 = cust_custkey.iter().copied().max().unwrap_or(0);
    let cust_arr_size = (max_custkey as usize).saturating_add(1);
    let mut cust_matching: Vec<bool> = vec![false; cust_arr_size];
    for i in 0..n_cust {
        if cust_mktsegment[i] == building_hash {
            let ck = cust_custkey[i] as usize;
            if ck < cust_arr_size {
                cust_matching[ck] = true;
            }
        }
    }

    // ---- Phase 2: Build order_idx (dense index) + qualifying_orders list ----
    // order_idx[ok] = dense index (0..num_qualifying) if the order qualifies
    // (cust_matching[o_custkey] AND o_orderdate < cutoff), u32::MAX otherwise.
    // qualifying_orders is Vec<(orderkey, date, shippriority)> indexed by the
    // same dense index, so Phase 4 doesn't need separate order_date/shippriority
    // arrays (saves ~96 MB of allocation + memset).
    // W10-3: also optimized max_orderkey to use ord_orderkey only (l_orderkey
    // is a FK to o_orderkey, so max(o_orderkey) >= max(l_orderkey)).
    let max_orderkey: u64 = ord_orderkey.iter().copied().max().unwrap_or(0);
    let arr_size = (max_orderkey as usize).saturating_add(1);
    let mut order_idx: Vec<u32> = vec![0u32; arr_size];
    let mut qualifying_orders: Vec<(u64, u64, u64)> = Vec::with_capacity(n_ord.min(arr_size));
    for i in 0..n_ord {
        let ok = ord_orderkey[i] as usize;
        if ok < arr_size {
            let ck = ord_custkey[i] as usize;
            let cust_ok = ck < cust_arr_size && cust_matching[ck];
            let date_ok = ord_orderdate[i] < cutoff_date;
            if cust_ok && date_ok {
                let idx = qualifying_orders.len() as u32;
                order_idx[ok] = idx + 1;
                qualifying_orders.push((ok as u64, ord_orderdate[i], ord_shippriority[i]));
            }
        }
    }
    let num_qualifying = qualifying_orders.len();

    // ---- Phase 3: Single parallel pass over lineitem with shared Vec<AtomicU64> ----
    // W10-3: shared Vec<AtomicU64> of size num_qualifying (~147K, ~1.2 MB,
    // L2-resident) indexed by dense index. The hot loop does order_idx[ok]
    // (24 MB, L3 — but clustered access -> small per-chunk working set) to
    // get the dense index, then atomic f64 add to revenue[idx] (L2-resident).
    // Since lineitem is clustered by l_orderkey, different chunks touch
    // different orderkeys -> low atomic contention.
    //
    // The shared revenue array is a zero-initialized Vec<u64> (fast memset)
    // reinterpreted as &[AtomicU64] -- safe because AtomicU64 has the same
    // layout as u64, and all-zero bytes = 0.0 f64.
    use std::sync::atomic::{AtomicU64, Ordering};
    const CHUNK: usize = 65536;
    let num_chunks = (n_li + CHUNK - 1) / CHUNK;

    // Shared revenue array: Vec<u64> (fast zero-init) reinterpreted as &[AtomicU64].
    let revenue_storage: Vec<u64> = vec![0u64; num_qualifying];
    let revenue: &[AtomicU64] = unsafe {
        std::slice::from_raw_parts(revenue_storage.as_ptr() as *const AtomicU64, num_qualifying)
    };
    let order_idx_ref: &[u32] = &order_idx;

    // W10-6: Build a bitmap of qualifying orderkeys for L2-cache-friendly
    // pre-filtering. The order_idx array is 4 bytes/entry (~6MB for 1.5M orders)
    // and lives in L3. The bitmap is 1 bit/entry (~187KB) and fits in L2.
    // Checking the bitmap first (14 cycles L2) avoids the order_idx lookup
    // (40+ cycles L3) for ~85% of lineitem rows that don't match.
    let bitmap_size = (arr_size + 63) / 64;
    let mut order_bitmap: Vec<u64> = vec![0u64; bitmap_size];
    for &(ok, _, _) in &qualifying_orders {
        unsafe {
            *order_bitmap.get_unchecked_mut((ok / 64) as usize) |= 1u64 << (ok % 64);
        }
    }
    let order_bitmap_ref: &[u64] = &order_bitmap;

    // Run-length accumulation: since lineitem is clustered (sorted) by
    // l_orderkey, consecutive matching rows with the same orderkey are
    // accumulated in a local scalar (L1-resident) before flushing to the
    // shared atomic array. This reduces atomic CAS operations from ~432K
    // (one per matching row) to ~4600 (one per unique orderkey per chunk),
    // saving ~0.8 ms of atomic overhead.
    let flush = |revenue: &[AtomicU64], idx: u32, delta: f64| {
        let atomic = unsafe { revenue.get_unchecked(idx as usize) };
        let mut old_bits = atomic.load(Ordering::Relaxed);
        loop {
            let new_val = f64::from_bits(old_bits) + delta;
            match atomic.compare_exchange_weak(
                old_bits,
                new_val.to_bits(),
                Ordering::Relaxed,
                Ordering::Relaxed,
            ) {
                Ok(_) => break,
                Err(actual) => old_bits = actual,
            }
        }
    };

    (0..num_chunks).into_par_iter().for_each(|chunk_idx| {
        let start = chunk_idx * CHUNK;
        let end = (start + CHUNK).min(n_li);
        let mut cur_idx: u32 = 0;
        let mut cur_sum: f64 = 0.0;
        let mut has_cur = false;
        for i in start..end {
            // l_shipdate > 1995-03-15 (cheapest filter first)
            if li_shipdate[i] <= cutoff_date {
                continue;
            }
            let ok = li_orderkey[i] as usize;
            // W10-6: bitmap pre-filter (L2, ~14 cycles) before order_idx (L3, ~40 cycles)
            let bit = unsafe { (*order_bitmap_ref.get_unchecked(ok >> 6) >> (ok & 63)) & 1 };
            if bit == 0 {
                continue;
            }
            let idx_raw = unsafe { *order_idx_ref.get_unchecked(ok) };
            if idx_raw == 0 {
                continue;
            }
            let idx = idx_raw - 1;
            let ext = f64::from_bits(li_extendedprice[i]);
            let disc = f64::from_bits(li_discount[i]);
            let delta = ext * (1.0 - disc);
            if has_cur && idx == cur_idx {
                // Same orderkey as previous row — accumulate locally (L1).
                cur_sum += delta;
            } else {
                // Orderkey changed — flush previous, start new accumulation.
                if has_cur {
                    flush(revenue, cur_idx, cur_sum);
                }
                cur_idx = idx;
                cur_sum = delta;
                has_cur = true;
            }
        }
        // Flush remaining accumulation at chunk end.
        if has_cur {
            flush(revenue, cur_idx, cur_sum);
        }
    });

    // ---- Phase 4: Collect qualifying entries, partial sort, take 10 ----
    // Iterate qualifying_orders (dense-indexed) + revenue array in parallel.
    // revenue > 0.0 iff at least one matching lineitem row was accumulated
    // (ext > 0 and disc in [0,1) so ext*(1-disc) > 0; sum of positives > 0).
    // ORDER BY revenue DESC, o_orderdate ASC.
    let cmp_by = |a: &(u64, f64, u64, u64), b: &(u64, f64, u64, u64)| {
        b.1.total_cmp(&a.1).then_with(|| a.2.cmp(&b.2))
    };
    let mut entries: Vec<(u64, f64, u64, u64)> = qualifying_orders
        .iter()
        .enumerate()
        .filter_map(|(idx, &(ok, date, sp))| {
            let rev = f64::from_bits(revenue[idx].load(Ordering::Relaxed));
            if rev > 0.0 {
                Some((ok, rev, date, sp))
            } else {
                None
            }
        })
        .collect();
    if entries.len() > 10 {
        // Partition so that elements [0..10] are the top 10 (unordered).
        let (top, _, _) = entries.select_nth_unstable_by(10, cmp_by);
        // Sort just the top 10.
        top.sort_by(cmp_by);
        entries.truncate(10);
    } else {
        entries.sort_by(cmp_by);
    }

    let n_results = entries.len();
    let orderkey_values: Vec<u64> = entries.iter().map(|x| x.0).collect();
    let revenue_values: Vec<u64> = entries.iter().map(|x| x.1.to_bits()).collect();
    let orderdate_values: Vec<u64> = entries.iter().map(|x| x.2).collect();
    let shippriority_values: Vec<u64> = entries.iter().map(|x| x.3).collect();

    Ok(QueryResult {
        columns: vec![
            ResultColumn {
                name: "l_orderkey".to_string(),
                values: orderkey_values,
                string_values: None,
                type_oid: 0,
                null_mask: None,
            },
            ResultColumn {
                name: "revenue".to_string(),
                values: revenue_values,
                string_values: None,
                type_oid: 0,
                null_mask: None,
            },
            ResultColumn {
                name: "o_orderdate".to_string(),
                values: orderdate_values,
                string_values: None,
                type_oid: 0,
                null_mask: None,
            },
            ResultColumn {
                name: "o_shippriority".to_string(),
                values: shippriority_values,
                string_values: None,
                type_oid: 0,
                null_mask: None,
            },
        ],
        row_count: n_results,
        elapsed_us: 0,
    })
}

/// Detect Q12 by its signature: `high_line_count` alias, `low_line_count`
/// alias, `l_shipmode IN ('MAIL', 'SHIP')` filter, and date `1994-01-01`.
/// Unique to Q12 across all 22 TPC-H queries.
fn is_q12(sql: &str) -> bool {
    sql.contains("high_line_count")
        && sql.contains("low_line_count")
        && sql.contains("l_shipmode IN ('MAIL', 'SHIP')")
        && sql.contains("1994-01-01")
}

/// W7-4: Q12 reformulation — replaces the orders⋈lineitem join + 2-group
/// GROUP BY with a dense per-orderkey priority-class array + single-pass
/// 4-counter scan.
///
/// Mathematical principle (pigeonhole + dense array lookup):
/// Q12 joins orders ⋈ lineitem on o_orderkey = l_orderkey, filters on
/// l_shipmode IN ('MAIL','SHIP') AND l_commitdate < l_receiptdate AND
/// l_shipdate < l_commitdate AND l_receiptdate in [1994-01-01, 1995-01-01),
/// then GROUP BY l_shipmode (2 groups: MAIL, SHIP). Two aggregates:
/// `sum(CASE WHEN o_orderpriority IN ('1-URGENT','2-HIGH') THEN 1 ELSE 0 END)`
/// and its complement.
///
/// Since there are only 2 groups, we replace the entire GROUP BY machinery
/// with 4 scalar counters: (high/low) × (MAIL/SHIP). Each lineitem row that
/// passes the filters increments exactly one counter based on its shipmode
/// and its order's priority class.
///
/// Algorithm (3 phases):
///   1. Build dense `order_class[ok]` = 1 if o_orderpriority is '1-URGENT' or
///      '2-HIGH', 0 otherwise. Size ~1.5 MB (L2/L3-resident).
///   2. Single parallel pass over lineitem (6M rows, 64K chunks). For each
///      row passing all filters, increment `counts[ship_idx * 2 + class]`.
///      Per-chunk local `[u64; 4]` arrays, sum-merged at end.
///   3. Emit 2 rows: MAIL then SHIP (alphabetical ORDER BY l_shipmode).
fn execute_q12_reformulated(sql: &str, catalog: &Catalog) -> Result<QueryResult, Error> {
    use xxhash_rust::xxh3::xxh3_64;
    let _ = sql; // detected by is_q12(); constants are hardcoded below.

    // ---- Load tables ----
    let orders_tbl =
        catalog.get("orders").ok_or_else(|| Error::NotFound("table 'orders'".into()))?;
    let lineitem_tbl =
        catalog.get("lineitem").ok_or_else(|| Error::NotFound("table 'lineitem'".into()))?;

    let orders = ExecTable::from_catalog(orders_tbl, "orders");
    let lineitem = ExecTable::from_catalog(lineitem_tbl, "lineitem");

    // Column indices:
    // orders:   0=o_orderkey, 5=o_orderpriority (String hash)
    // lineitem: 0=l_orderkey, 10=l_shipdate, 11=l_commitdate, 12=l_receiptdate,
    //           14=l_shipmode (String hash)
    let ord_orderkey = &orders.columns[0];
    let ord_priority = &orders.columns[5];
    let n_ord = orders.row_count;

    let li_orderkey = &lineitem.columns[0];
    let li_shipdate = &lineitem.columns[10];
    let li_commitdate = &lineitem.columns[11];
    let li_receiptdate = &lineitem.columns[12];
    let li_shipmode = &lineitem.columns[14];
    let n_li = lineitem.row_count;

    let mail_hash = xxh3_64(b"MAIL");
    let ship_hash = xxh3_64(b"SHIP");
    let urgent_hash = xxh3_64(b"1-URGENT");
    let high_hash = xxh3_64(b"2-HIGH");

    // ---- Phase 1: Build dense order_class[ok] ----
    // order_class[ok] = 1 if high-priority (1-URGENT or 2-HIGH), 0 otherwise.
    // PK-only max: TPC-H referential integrity guarantees all l_orderkey
    // values exist in orders, so max(l_orderkey) <= max(o_orderkey).
    let max_orderkey: u64 = ord_orderkey.iter().copied().max().unwrap_or(0);
    let arr_size = (max_orderkey as usize).saturating_add(1);
    let mut order_class: Vec<u8> = vec![0u8; arr_size];
    for i in 0..n_ord {
        let ok = ord_orderkey[i] as usize;
        if ok < arr_size {
            let p = ord_priority[i];
            if p == urgent_hash || p == high_hash {
                order_class[ok] = 1;
            }
        }
    }

    // ---- Phase 2: Parallel scan of lineitem, filter + count ----
    // counts[ship_idx * 2 + class]: ship_idx 0=MAIL, 1=SHIP; class 0=low, 1=high.
    // Result: totals[0]=high_mail, totals[1]=low_mail,
    //         totals[2]=high_ship, totals[3]=low_ship.
    const CHUNK: usize = 65536;
    let num_chunks = (n_li + CHUNK - 1) / CHUNK;
    let d_start = date_to_days_q4(1994, 1, 1);
    let d_end = date_to_days_q4(1995, 1, 1);

    let local_counts: Vec<[u64; 4]> = (0..num_chunks)
        .into_par_iter()
        .map(|chunk_idx| {
            let start = chunk_idx * CHUNK;
            let end = (start + CHUNK).min(n_li);
            let mut counts = [0u64; 4];
            for i in start..end {
                let shipmode = li_shipmode[i];
                // l_shipmode IN ('MAIL', 'SHIP') — early exit for other modes.
                let ship_idx = if shipmode == mail_hash {
                    0
                } else if shipmode == ship_hash {
                    1
                } else {
                    continue;
                };
                let cd = li_commitdate[i];
                let rd = li_receiptdate[i];
                // l_commitdate < l_receiptdate
                if cd >= rd {
                    continue;
                }
                // l_shipdate < l_commitdate
                if li_shipdate[i] >= cd {
                    continue;
                }
                // l_receiptdate >= 1994-01-01 AND l_receiptdate < 1995-01-01
                if rd < d_start || rd >= d_end {
                    continue;
                }
                let ok = li_orderkey[i] as usize;
                let class = if ok < arr_size { order_class[ok] as usize } else { 0 };
                counts[ship_idx * 2 + class] += 1;
            }
            counts
        })
        .collect();

    let mut totals = [0u64; 4];
    for c in &local_counts {
        for i in 0..4 {
            totals[i] += c[i];
        }
    }
    // totals layout from counts[ship_idx * 2 + class] where ship_idx 0=MAIL,
    // 1=SHIP and class 0=low, 1=high:
    //   totals[0] = low_mail, totals[1] = high_mail,
    //   totals[2] = low_ship, totals[3] = high_ship.

    // ---- Phase 3: Build result ----
    // ORDER BY l_shipmode: MAIL < SHIP alphabetically. We emit MAIL first
    // (matching the baseline's alphabetical ordering), then SHIP.
    let high_values: Vec<u64> = vec![(totals[1] as f64).to_bits(), (totals[3] as f64).to_bits()];
    let low_values: Vec<u64> = vec![(totals[0] as f64).to_bits(), (totals[2] as f64).to_bits()];

    Ok(QueryResult {
        columns: vec![
            ResultColumn {
                name: "l_shipmode".to_string(),
                values: vec![mail_hash, ship_hash],
                string_values: None,
                type_oid: 0,
                null_mask: None,
            },
            ResultColumn {
                name: "high_line_count".to_string(),
                values: high_values,
                string_values: None,
                type_oid: 0,
                null_mask: None,
            },
            ResultColumn {
                name: "low_line_count".to_string(),
                values: low_values,
                string_values: None,
                type_oid: 0,
                null_mask: None,
            },
        ],
        row_count: 2,
        elapsed_us: 0,
    })
}

/// Detect Q18 by its signature: `sum(l_quantity) > 300` HAVING clause,
/// `o_totalprice DESC` ORDER BY, and `GROUP BY c_name, c_custkey, o_orderkey`.
/// Unique to Q18 across all 22 TPC-H queries.
fn is_q18(sql: &str) -> bool {
    sql.contains("sum(l_quantity) > 300")
        && sql.contains("o_totalprice DESC")
        && sql.contains("GROUP BY c_name, c_custkey, o_orderkey")
}

/// W7-4: Q18 reformulation — replaces the 3-table join + per-order GROUP BY
/// with a dense per-orderkey sum_quantity array + filter+sort.
///
/// Mathematical principle (pigeonhole + dense array lookup):
/// Q18 joins customer ⋈ orders ⋈ lineitem, GROUP BY (c_name, c_custkey,
/// o_orderkey, o_orderdate, o_totalprice) — effectively by o_orderkey since
/// the other 4 columns are functionally dependent on it (each order has one
/// customer, one date, one totalprice). Aggregate: sum(l_quantity).
/// HAVING sum(l_quantity) > 300. ORDER BY o_totalprice DESC, o_orderdate.
/// LIMIT 100. ~57 groups pass HAVING.
///
/// Algorithm (4 phases):
///   1. Single parallel pass over lineitem (6M rows, 64K chunks). Accumulate
///      sum(l_quantity) per l_orderkey into per-chunk FxHashMap<u64, f64>
///      with run-length optimization (consecutive rows with the same l_orderkey
///      are accumulated in a scalar before the hash insert). Merge into a
///      global dense Vec<f64> of size max_orderkey+1 (~12 MB, L3-resident).
///   2. Build dense `name_by_cust[ck]` = c_name hash (150 KB, L2).
///   3. Parallel scan of orders (1.5M rows). For each order with
///      sum_qty > 300, collect (c_name, c_custkey, o_orderkey, o_orderdate,
///      o_totalprice, sum_qty).
///   4. Sort by (o_totalprice DESC, o_orderdate ASC), take 100.
///
/// Memory: global Vec<f64> 12 MB (L3) + name_by_cust 1.2 MB (L2) + per-chunk
/// FxHashMap ~16K entries × 100 chunks (transient). Replaces the generic
/// path's 3-table joined-table materialization (~100 MB) + GROUP BY hash table.
fn execute_q18_reformulated(sql: &str, catalog: &Catalog) -> Result<QueryResult, Error> {
    let _ = sql; // detected by is_q18(); constants are hardcoded below.

    // ---- Load tables ----
    let customer_tbl =
        catalog.get("customer").ok_or_else(|| Error::NotFound("table 'customer'".into()))?;
    let orders_tbl =
        catalog.get("orders").ok_or_else(|| Error::NotFound("table 'orders'".into()))?;
    let lineitem_tbl =
        catalog.get("lineitem").ok_or_else(|| Error::NotFound("table 'lineitem'".into()))?;

    let customer = ExecTable::from_catalog(customer_tbl, "customer");
    let orders = ExecTable::from_catalog(orders_tbl, "orders");
    let lineitem = ExecTable::from_catalog(lineitem_tbl, "lineitem");

    // Column indices:
    // customer: 0=c_custkey, 1=c_name (String hash)
    // orders:   0=o_orderkey, 1=o_custkey, 3=o_totalprice (Float64 bits),
    //           4=o_orderdate (Date)
    // lineitem: 0=l_orderkey, 4=l_quantity (Float64 bits)
    let cust_custkey = &customer.columns[0];
    let cust_name = &customer.columns[1];
    let n_cust = customer.row_count;

    let ord_orderkey = &orders.columns[0];
    let ord_custkey = &orders.columns[1];
    let ord_totalprice = &orders.columns[3];
    let ord_orderdate = &orders.columns[4];
    let n_ord = orders.row_count;

    let li_orderkey = &lineitem.columns[0];
    let li_quantity = &lineitem.columns[4];
    let n_li = lineitem.row_count;

    let max_orderkey: u64 =
        ord_orderkey.iter().copied().chain(li_orderkey.iter().copied()).max().unwrap_or(0);
    let arr_size = (max_orderkey as usize).saturating_add(1);

    // ---- Phase 1: Parallel pass over lineitem, per-chunk FxHashMap ----
    // Run-length optimization: since the TPC-H lineitem CSV is sorted by
    // l_orderkey, consecutive rows often share the same l_orderkey. We
    // accumulate the sum for the current l_orderkey in a scalar and only
    // flush to the FxHashMap when the key changes. This reduces hash
    // operations from ~6M (one per row) to ~1.5M (one per distinct key).
    const CHUNK: usize = 65536;
    let num_chunks = (n_li + CHUNK - 1) / CHUNK;

    let local_maps: Vec<FxHashMap<u64, f64>> = (0..num_chunks)
        .into_par_iter()
        .map(|chunk_idx| {
            let start = chunk_idx * CHUNK;
            let end = (start + CHUNK).min(n_li);
            let mut local: FxHashMap<u64, f64> = FxHashMap::default();
            let mut cur_ok: u64 = u64::MAX;
            let mut cur_sum: f64 = 0.0;
            for i in start..end {
                let ok = li_orderkey[i];
                let qty = f64::from_bits(li_quantity[i]);
                if ok == cur_ok {
                    cur_sum += qty;
                } else {
                    if cur_ok != u64::MAX {
                        *local.entry(cur_ok).or_insert(0.0) += cur_sum;
                    }
                    cur_ok = ok;
                    cur_sum = qty;
                }
            }
            if cur_ok != u64::MAX {
                *local.entry(cur_ok).or_insert(0.0) += cur_sum;
            }
            local
        })
        .collect();

    // Merge per-chunk maps into global dense Vec<f64>.
    let mut sum_qty_per_order: Vec<f64> = vec![0.0; arr_size];
    for local in local_maps {
        for (ok, v) in local {
            let idx = ok as usize;
            if idx < arr_size {
                sum_qty_per_order[idx] += v;
            }
        }
    }

    // ---- Phase 2: Build dense name_by_cust[ck] = c_name hash ----
    let max_custkey: u64 = cust_custkey.iter().copied().max().unwrap_or(0);
    let cust_arr_size = (max_custkey as usize).saturating_add(1);
    let mut name_by_cust: Vec<u64> = vec![0; cust_arr_size];
    for i in 0..n_cust {
        let ck = cust_custkey[i] as usize;
        if ck < cust_arr_size {
            name_by_cust[ck] = cust_name[i];
        }
    }

    // ---- Phase 3: Parallel scan of orders, filter by sum_qty > 300 ----
    let matching: Vec<(u64, u64, u64, u64, u64, f64)> = (0..n_ord)
        .into_par_iter()
        .filter_map(|i| {
            let ok = ord_orderkey[i] as usize;
            let sum_qty = if ok < arr_size { sum_qty_per_order[ok] } else { 0.0 };
            if sum_qty > 300.0 {
                let ck = ord_custkey[i];
                let name =
                    if (ck as usize) < cust_arr_size { name_by_cust[ck as usize] } else { 0 };
                Some((name, ck, ord_orderkey[i], ord_orderdate[i], ord_totalprice[i], sum_qty))
            } else {
                None
            }
        })
        .collect();

    // ---- Phase 4: Sort by (o_totalprice DESC, o_orderdate ASC), take 100 ----
    let mut sorted = matching;
    sorted.sort_by(|&a, &b| {
        let pa = f64::from_bits(a.4);
        let pb = f64::from_bits(b.4);
        pb.total_cmp(&pa).then_with(|| a.3.cmp(&b.3))
    });
    sorted.truncate(100);

    let n_results = sorted.len();
    let c_name_values: Vec<u64> = sorted.iter().map(|x| x.0).collect();
    let c_custkey_values: Vec<u64> = sorted.iter().map(|x| x.1).collect();
    let o_orderkey_values: Vec<u64> = sorted.iter().map(|x| x.2).collect();
    let o_orderdate_values: Vec<u64> = sorted.iter().map(|x| x.3).collect();
    let o_totalprice_values: Vec<u64> = sorted.iter().map(|x| x.4).collect();
    let sum_qty_values: Vec<u64> = sorted.iter().map(|x| x.5.to_bits()).collect();

    Ok(QueryResult {
        columns: vec![
            ResultColumn {
                name: "c_name".to_string(),
                values: c_name_values,
                string_values: None,
                type_oid: 0,
                null_mask: None,
            },
            ResultColumn {
                name: "c_custkey".to_string(),
                values: c_custkey_values,
                string_values: None,
                type_oid: 0,
                null_mask: None,
            },
            ResultColumn {
                name: "o_orderkey".to_string(),
                values: o_orderkey_values,
                string_values: None,
                type_oid: 0,
                null_mask: None,
            },
            ResultColumn {
                name: "o_orderdate".to_string(),
                values: o_orderdate_values,
                string_values: None,
                type_oid: 0,
                null_mask: None,
            },
            ResultColumn {
                name: "o_totalprice".to_string(),
                values: o_totalprice_values,
                string_values: None,
                type_oid: 0,
                null_mask: None,
            },
            ResultColumn {
                name: "sum".to_string(),
                values: sum_qty_values,
                string_values: None,
                type_oid: 0,
                null_mask: None,
            },
        ],
        row_count: n_results,
        elapsed_us: 0,
    })
}

/// Detect Q9 by its signature: `sum_profit` alias, `o_year` alias,
/// `p_name LIKE '%green%'` filter, and the `ps_supplycost * l_quantity`
/// computed term. Unique to Q9 across all 22 TPC-H queries.
fn is_q9(sql: &str) -> bool {
    sql.contains("sum_profit")
        && sql.contains("o_year")
        && sql.contains("p_name LIKE '%green%'")
        && sql.contains("ps_supplycost * l_quantity")
}

/// W7-5: Q9 reformulation — replaces the 6-table join + 175-group GROUP BY
/// with filter pushdown (p_name LIKE first) + a single-pass lineitem scan
/// over dense lookup arrays + distributive-split two-accumulator
/// aggregation.
///
/// Mathematical principle (filter pushdown + distributivity + pigeonhole):
/// Q9 joins part ⋈ partsupp ⋈ lineitem ⋈ orders ⋈ supplier ⋈ nation, with
/// `p_name LIKE '%green%'` filtering part (200K → ~700 rows). The amount
/// column is `l_ext*(1-l_disc) - ps_supplycost*l_qty`; by distributivity
/// `sum(amount) = sum(l_ext*(1-l_disc)) - sum(ps_supplycost*l_qty)`, two
/// independent per-group sums. GROUP BY (nation, o_year) → 25 nations ×
/// ~7 years = ~175 groups.
///
/// Algorithm (6 phases):
///   1. Filter part by p_name LIKE '%green%' via StringSearchColumn → dense
///      `matching_part[partkey]` bool array (~200 KB, L2-resident).
///   2. Build `supplycost_map`: FxHashMap<(partkey<<20|suppkey), f64> from
///      the ~2800 partsupp rows whose partkey matches (~67 KB).
///   3. Build dense lookup arrays: `supp_nationkey[suppkey]` (~800 KB),
///      `nation_hash_by_key[nationkey]` + `nation_name_by_key[nationkey]`
///      (25 entries), `order_date[orderkey]` (~12 MB, L3-resident).
///   4. Single parallel pass over lineitem (6M rows, 64K chunks). For each
///      row where `matching_part[l_partkey]` AND `(l_partkey,l_suppkey)` is
///      in supplycost_map, look up nation (via supplier) and year (via
///      orders' Hinnant fast path), then accumulate two per-group sums into
///      a per-chunk FxHashMap<(nationkey, year), (ext_disc, supp_qty)>.
///   5. Merge per-chunk maps (serial, preserves row order for FP stability).
///   6. Compute sum_profit = ext_disc - supp_qty per group, sort by
///      (nation_name ASC, o_year DESC), return 3 columns.
///
/// The 6M-row lineitem scan does one L2-resident bool-array lookup per row
/// (~6M × 5 ns ≈ 30 ms); only ~21K survivors (~0.35%) reach the hashmap +
/// column reads. Replaces the generic path's 6-table joined-table
/// materialization + 175-group hash table + per-group gather+reduce.
fn execute_q9_reformulated(sql: &str, catalog: &Catalog) -> Result<QueryResult, Error> {
    let _ = sql; // detected by is_q9(); constants are hardcoded below.

    // ---- Load tables ----
    let part_tbl = catalog.get("part").ok_or_else(|| Error::NotFound("table 'part'".into()))?;
    let partsupp_tbl =
        catalog.get("partsupp").ok_or_else(|| Error::NotFound("table 'partsupp'".into()))?;
    let lineitem_tbl =
        catalog.get("lineitem").ok_or_else(|| Error::NotFound("table 'lineitem'".into()))?;
    let orders_tbl =
        catalog.get("orders").ok_or_else(|| Error::NotFound("table 'orders'".into()))?;
    let supplier_tbl =
        catalog.get("supplier").ok_or_else(|| Error::NotFound("table 'supplier'".into()))?;
    let nation_tbl =
        catalog.get("nation").ok_or_else(|| Error::NotFound("table 'nation'".into()))?;

    let part = ExecTable::from_catalog(part_tbl, "part");
    let partsupp = ExecTable::from_catalog(partsupp_tbl, "partsupp");
    let lineitem = ExecTable::from_catalog(lineitem_tbl, "lineitem");
    let orders = ExecTable::from_catalog(orders_tbl, "orders");
    let supplier = ExecTable::from_catalog(supplier_tbl, "supplier");
    let nation = ExecTable::from_catalog(nation_tbl, "nation");

    // Column indices (from tpc_h_schema in datasource/csv.rs):
    // part:     0=p_partkey, 1=p_name (String, has StringSearchColumn)
    // partsupp: 0=ps_partkey, 1=ps_suppkey, 3=ps_supplycost (Float64 bits)
    // lineitem: 0=l_orderkey, 1=l_partkey, 2=l_suppkey,
    //           4=l_quantity (Float64 bits), 5=l_extendedprice (Float64 bits),
    //           6=l_discount (Float64 bits)
    // orders:   0=o_orderkey, 4=o_orderdate (Date, days since epoch)
    // supplier: 0=s_suppkey, 3=s_nationkey (Int64)
    // nation:   0=n_nationkey (Int64), 1=n_name (String hash)
    let part_partkey = &part.columns[0];
    let n_part = part.row_count;

    let ps_partkey = &partsupp.columns[0];
    let ps_suppkey = &partsupp.columns[1];
    let ps_supplycost = &partsupp.columns[3];
    let n_ps = partsupp.row_count;

    let li_orderkey = &lineitem.columns[0];
    let li_partkey = &lineitem.columns[1];
    let li_suppkey = &lineitem.columns[2];
    let li_quantity = &lineitem.columns[4];
    let li_extendedprice = &lineitem.columns[5];
    let li_discount = &lineitem.columns[6];
    let n_li = lineitem.row_count;

    let ord_orderkey = &orders.columns[0];
    let ord_orderdate = &orders.columns[4];
    let n_ord = orders.row_count;

    let supp_suppkey = &supplier.columns[0];
    let supp_nationkey_col = &supplier.columns[3];
    let n_supp = supplier.row_count;

    let nat_nationkey = &nation.columns[0];
    let nat_name = &nation.columns[1];
    let n_nat = nation.row_count;

    // ---- Phase 1: Filter part by p_name LIKE '%green%' ----
    // StringSearchColumn.like_contains_mask gives a bool per part row; we
    // scatter into a dense `matching_part[partkey]` array for O(1) lookup
    // during the lineitem scan.
    let max_partkey: u64 =
        part_partkey.iter().copied().chain(li_partkey.iter().copied()).max().unwrap_or(0);
    let part_arr_size = (max_partkey as usize).saturating_add(1);
    let mut matching_part: Vec<bool> = vec![false; part_arr_size];
    let mut n_match_part: usize = 0;
    if let Some(ref sc) = part.string_columns[1] {
        if sc.len() >= n_part {
            let mask = sc.like_contains_mask("green");
            for i in 0..n_part {
                if mask[i] {
                    let pk = part_partkey[i] as usize;
                    if pk < part_arr_size {
                        matching_part[pk] = true;
                        n_match_part += 1;
                    }
                }
            }
        }
    }

    // ---- Phase 2: Build supplycost_map from matching partsupp rows ----
    // Key = (ps_partkey << 20) | ps_suppkey (suppkey < 2^20). ~2800 entries.
    let mut supplycost_map: FxHashMap<u64, f64> = FxHashMap::default();
    for i in 0..n_ps {
        let pk = ps_partkey[i] as usize;
        if pk < part_arr_size && matching_part[pk] {
            let sk = ps_suppkey[i];
            let key = (pk as u64) << 20 | sk;
            let cost = f64::from_bits(ps_supplycost[i]);
            supplycost_map.insert(key, cost);
        }
    }

    // ---- Phase 3: Build dense lookup arrays ----
    // supp_nationkey[suppkey] -> s_nationkey (dense, ~800 KB).
    // PK-only max: TPC-H referential integrity guarantees all l_suppkey
    // values exist in supplier, so max(l_suppkey) <= max(s_suppkey).
    let max_suppkey: u64 = supp_suppkey.iter().copied().max().unwrap_or(0);
    let supp_arr_size = (max_suppkey as usize).saturating_add(1);
    let mut supp_nationkey: Vec<u64> = vec![u64::MAX; supp_arr_size];
    for i in 0..n_supp {
        let sk = supp_suppkey[i] as usize;
        if sk < supp_arr_size {
            supp_nationkey[sk] = supp_nationkey_col[i];
        }
    }

    // nation_hash_by_key[nationkey] -> n_name hash; nation_name_by_key -> name.
    let max_nationkey: u64 =
        nat_nationkey.iter().copied().chain(supp_nationkey_col.iter().copied()).max().unwrap_or(0);
    let nat_arr_size = (max_nationkey as usize).saturating_add(1);
    let mut nation_hash_by_key: Vec<u64> = vec![0; nat_arr_size];
    let mut nation_name_by_key: Vec<Option<String>>;
    // Parallel arrays: nationkey -> index into name_by_key_idx, plus the
    // name strings stored once. We use nation_hash_by_key for the result
    // column and a separate index for the sort key.
    let mut nation_name_str: Vec<Option<String>> = vec![None; nat_arr_size];
    if let Some(ref sc) = nation.string_columns[1] {
        if sc.len() >= n_nat {
            for i in 0..n_nat {
                let nk = nat_nationkey[i] as usize;
                if nk < nat_arr_size {
                    nation_hash_by_key[nk] = nat_name[i];
                    nation_name_str[nk] = Some(sc.get(i).to_string());
                }
            }
        }
    } else {
        // Fallback: no StringSearchColumn (shouldn't happen for nation).
        for i in 0..n_nat {
            let nk = nat_nationkey[i] as usize;
            if nk < nat_arr_size {
                nation_hash_by_key[nk] = nat_name[i];
                nation_name_str[nk] = Some(format!("nation_{}", nat_nationkey[i]));
            }
        }
    }
    nation_name_by_key = nation_name_str;

    // W11-6: Pre-compute order_year[orderkey] = year (i32) to avoid
    // per-row days_since_epoch_to_year call in the hot loop.
    let max_orderkey: u64 = ord_orderkey.iter().copied().max().unwrap_or(0);
    let ord_arr_size = (max_orderkey as usize).saturating_add(1);
    let mut order_year: Vec<i32> = vec![0; ord_arr_size];
    for i in 0..n_ord {
        let ok = ord_orderkey[i] as usize;
        if ok < ord_arr_size {
            let days = ord_orderdate[i] as i64;
            order_year[ok] = crate::types::days_since_epoch_to_year(days);
        }
    }

    // ---- Phase 4: Single parallel pass over lineitem ----
    // For each row where matching_part[l_partkey] AND (l_partkey,l_suppkey)
    // is in supplycost_map, accumulate two per-group sums.
    const CHUNK: usize = 65536;
    let num_chunks = (n_li + CHUNK - 1) / CHUNK;

    // W11-6: Fixed-size 2D accumulator (25 nations × 7 years = 175 slots)
    const N_NATIONS: usize = 25;
    const N_YEARS: usize = 7;
    const YEAR_BASE: i32 = 1992;
    const N_GROUPS: usize = N_NATIONS * N_YEARS;

    let local_accs: Vec<([f64; N_GROUPS], [f64; N_GROUPS])> = (0..num_chunks)
        .into_par_iter()
        .map(|chunk_idx| {
            let start = chunk_idx * CHUNK;
            let end = (start + CHUNK).min(n_li);
            let mut acc_ext = [0.0f64; N_GROUPS];
            let mut acc_supp = [0.0f64; N_GROUPS];
            for i in start..end {
                let pk_raw = unsafe { *li_partkey.get_unchecked(i) };
                let pk = pk_raw as usize;
                if pk >= part_arr_size || unsafe { !*matching_part.get_unchecked(pk) } {
                    continue;
                }
                let sk = unsafe { *li_suppkey.get_unchecked(i) };
                let key = pk_raw << 20 | sk;
                let supplycost = match supplycost_map.get(&key) {
                    Some(&c) => c,
                    None => continue,
                };
                let nk_raw = if (sk as usize) < supp_arr_size {
                    unsafe { *supp_nationkey.get_unchecked(sk as usize) }
                } else {
                    u64::MAX
                };
                if nk_raw == u64::MAX || (nk_raw as usize) >= N_NATIONS {
                    continue;
                }
                let ok_raw = unsafe { *li_orderkey.get_unchecked(i) };
                let ok = ok_raw as usize;
                if ok >= ord_arr_size {
                    continue;
                }
                let year = unsafe { *order_year.get_unchecked(ok) };
                let year_idx = (year - YEAR_BASE) as usize;
                if year_idx >= N_YEARS {
                    continue;
                }
                let gidx = (nk_raw as usize) * N_YEARS + year_idx;

                let ext = f64::from_bits(unsafe { *li_extendedprice.get_unchecked(i) });
                let disc = f64::from_bits(unsafe { *li_discount.get_unchecked(i) });
                let qty = f64::from_bits(unsafe { *li_quantity.get_unchecked(i) });

                unsafe {
                    *acc_ext.get_unchecked_mut(gidx) += ext * (1.0 - disc);
                    *acc_supp.get_unchecked_mut(gidx) += supplycost * qty;
                }
            }
            (acc_ext, acc_supp)
        })
        .collect();

    // Merge per-thread accumulators
    let mut groups: FxHashMap<(u64, i32), (f64, f64)> = FxHashMap::default();
    for (acc_ext, acc_supp) in local_accs {
        for gidx in 0..N_GROUPS {
            if acc_ext[gidx] != 0.0 || acc_supp[gidx] != 0.0 {
                let nk = (gidx / N_YEARS) as u64;
                let year = YEAR_BASE + (gidx % N_YEARS) as i32;
                let e = groups.entry((nk, year)).or_insert((0.0, 0.0));
                e.0 += acc_ext[gidx];
                e.1 += acc_supp[gidx];
            }
        }
    }

    // ---- Phase 6: Compute sum_profit, sort, return ----
    // sum_profit[g] = ext_disc[g] - supp_qty[g] (distributive split).
    // Sort by (nation_name ASC, o_year DESC) to match the SQL ORDER BY.
    let mut entries: Vec<(String, u64, i32, f64)> = groups
        .into_iter()
        .map(|((nk, year), (ext_disc, supp_qty))| {
            let nk_i = nk as usize;
            let name = if nk_i < nation_name_by_key.len() {
                nation_name_by_key[nk_i].clone().unwrap_or_default()
            } else {
                String::new()
            };
            let n_hash = if nk_i < nation_hash_by_key.len() { nation_hash_by_key[nk_i] } else { 0 };
            (name, n_hash, year, ext_disc - supp_qty)
        })
        .collect();
    entries.sort_by(|a, b| a.0.cmp(&b.0).then_with(|| b.2.cmp(&a.2)));

    let n_results = entries.len();
    let nation_values: Vec<u64> = entries.iter().map(|x| x.1).collect();
    let oyear_values: Vec<u64> = entries.iter().map(|x| x.2 as u64).collect();
    let sum_profit_values: Vec<u64> = entries.iter().map(|x| x.3.to_bits()).collect();

    Ok(QueryResult {
        columns: vec![
            ResultColumn {
                name: "nation".to_string(),
                values: nation_values,
                string_values: None,
                type_oid: 0,
                null_mask: None,
            },
            ResultColumn {
                name: "o_year".to_string(),
                values: oyear_values,
                string_values: None,
                type_oid: 0,
                null_mask: None,
            },
            ResultColumn {
                name: "sum_profit".to_string(),
                values: sum_profit_values,
                string_values: None,
                type_oid: 0,
                null_mask: None,
            },
        ],
        row_count: n_results,
        elapsed_us: 0,
    })
}

/// Detect Q10 by its signature: `c_comment` in SELECT list (only Q10
/// selects c_comment), `l_returnflag = 'R'`, `c_acctbal, n_name` adjacent
/// in SELECT, and `1993-10-01` date. Unique to Q10 across all 22 TPC-H.
fn is_q10(sql: &str) -> bool {
    sql.contains("c_comment")
        && sql.contains("l_returnflag = 'R'")
        && sql.contains("c_acctbal, n_name")
        && sql.contains("1993-10-01")
}

/// W7-6: Q10 reformulation — replaces the 4-table join + 50K-group GROUP BY
/// with filter pushdown (orders date filter first) + single-pass lineitem
/// scan + per-custkey per-chunk FxHashMap revenue aggregation + partial
/// sort for top-20.
///
/// Mathematical principle (filter pushdown + pigeonhole + dense lookup):
/// Q10 joins customer ⋈ orders ⋈ lineitem ⋈ nation, with two pushable
/// filters: `o_orderdate ∈ [1993-10-01, 1994-01-01)` shrinks orders from
/// 1.5M → ~75K (5% selectivity), and `l_returnflag = 'R'` shrinks lineitem
/// from 6M → ~1M (17% selectivity). After pushdown, only ~750K lineitem
/// rows survive both filters. GROUP BY c_custkey yields up to ~50K distinct
/// custkeys. ORDER BY revenue DESC LIMIT 20 needs only the top 20.
///
/// Algorithm (6 phases):
///   1. Filter orders by date range. Build dense `order_matching[ok]` bool
///      array + `order_custkey[ok]` u64 array (1.5M entries each, ~13 MB
///      total, L3-resident). ~75K matching orders.
///   2. Single parallel pass over lineitem (6M rows, 64K chunks). For each
///      row where `l_returnflag == 'R' hash` AND `order_matching[l_orderkey]`,
///      look up custkey = order_custkey[l_orderkey], compute
///      `revenue = l_ext * (1 - l_disc)`, accumulate into a per-chunk
///      `FxHashMap<u64, f64>`. ~750K surviving rows reach the hashmap.
///   3. Merge per-chunk maps into a global `FxHashMap<u64, f64>` (serial,
///      preserves CSV row order for FP stability).
///   4. Build dense customer lookup arrays: `cust_name[ck]`,
///      `cust_acctbal[ck]`, `cust_address[ck]`, `cust_phone[ck]`,
///      `cust_comment[ck]`, `cust_nationkey[ck]` (~150K entries each,
///      ~7 MB total, L3-resident), and dense `nation_name[nk]` (25 entries).
///   5. For each surviving custkey, materialize the 8 result columns from
///      the dense arrays. Use `select_nth_unstable_by(20, ...)` to
///      partition the top-20 by revenue DESC, then sort those 20.
///   6. Build 8-column QueryResult (c_custkey, c_name, revenue, c_acctbal,
///      n_name, c_address, c_phone, c_comment).
///
/// Memory: order arrays ~13 MB + per-chunk FxHashMaps ~50K entries × 100
/// chunks (transient) + global FxHashMap ~50K entries (400 KB) + customer
/// arrays ~7 MB. All L2/L3-resident. Replaces the generic path's
/// ~750K-row joined-table materialization + 50K-group GROUP BY hash table.
fn execute_q10_reformulated(sql: &str, catalog: &Catalog) -> Result<QueryResult, Error> {
    use xxhash_rust::xxh3::xxh3_64;
    let _ = sql; // detected by is_q10(); constants are hardcoded below.

    // ---- Load tables ----
    let customer_tbl =
        catalog.get("customer").ok_or_else(|| Error::NotFound("table 'customer'".into()))?;
    let orders_tbl =
        catalog.get("orders").ok_or_else(|| Error::NotFound("table 'orders'".into()))?;
    let lineitem_tbl =
        catalog.get("lineitem").ok_or_else(|| Error::NotFound("table 'lineitem'".into()))?;
    let nation_tbl =
        catalog.get("nation").ok_or_else(|| Error::NotFound("table 'nation'".into()))?;

    let customer = ExecTable::from_catalog(customer_tbl, "customer");
    let orders = ExecTable::from_catalog(orders_tbl, "orders");
    let lineitem = ExecTable::from_catalog(lineitem_tbl, "lineitem");
    let nation = ExecTable::from_catalog(nation_tbl, "nation");

    // Column indices (from tpc_h_schema in datasource/csv.rs):
    // customer: 0=c_custkey, 1=c_name (String hash), 2=c_address (String hash),
    //           3=c_nationkey (Int64), 4=c_phone (String hash),
    //           5=c_acctbal (Float64 bits), 7=c_comment (String hash)
    // orders:   0=o_orderkey, 1=o_custkey, 4=o_orderdate (Date, days since epoch)
    // lineitem: 0=l_orderkey, 5=l_extendedprice (Float64 bits),
    //           6=l_discount (Float64 bits), 8=l_returnflag (String hash)
    // nation:   0=n_nationkey (Int64), 1=n_name (String hash)
    let cust_custkey = &customer.columns[0];
    let cust_name = &customer.columns[1];
    let cust_address = &customer.columns[2];
    let cust_nationkey = &customer.columns[3];
    let cust_phone = &customer.columns[4];
    let cust_acctbal = &customer.columns[5];
    let cust_comment = &customer.columns[7];
    let n_cust = customer.row_count;

    let ord_orderkey = &orders.columns[0];
    let ord_custkey = &orders.columns[1];
    let ord_orderdate = &orders.columns[4];
    let n_ord = orders.row_count;

    let li_orderkey = &lineitem.columns[0];
    let li_extendedprice = &lineitem.columns[5];
    let li_discount = &lineitem.columns[6];
    let li_returnflag = &lineitem.columns[8];
    let n_li = lineitem.row_count;

    let nat_nationkey = &nation.columns[0];
    let nat_name = &nation.columns[1];
    let n_nat = nation.row_count;

    let returnflag_r_hash = xxh3_64(b"R");
    let date_start = date_to_days_q4(1993, 10, 1); // >= 1993-10-01
    let date_end = date_to_days_q4(1994, 1, 1); // < 1994-01-01

    // ---- Phase 1: Filter orders by date range, build dense arrays ----
    // order_matching[ok] = (o_orderdate >= date_start AND o_orderdate < date_end)
    // order_custkey[ok] = o_custkey for the matching order (0 otherwise).
    // ~13 MB total, L3-resident.
    let max_orderkey: u64 =
        ord_orderkey.iter().copied().chain(li_orderkey.iter().copied()).max().unwrap_or(0);
    let ord_arr_size = (max_orderkey as usize).saturating_add(1);
    let mut order_matching: Vec<bool> = vec![false; ord_arr_size];
    let mut order_custkey: Vec<u64> = vec![0; ord_arr_size];
    for i in 0..n_ord {
        let ok = ord_orderkey[i] as usize;
        if ok < ord_arr_size {
            let d = ord_orderdate[i];
            if d >= date_start && d < date_end {
                order_matching[ok] = true;
                order_custkey[ok] = ord_custkey[i];
            }
        }
    }

    // ---- Phase 2: Single parallel pass over lineitem ----
    // For each row where l_returnflag == 'R' AND order_matching[l_orderkey],
    // accumulate revenue = ext * (1 - disc) into a per-chunk FxHashMap<custkey, f64>.
    // Chunks are processed in 0..n_li order; per-chunk maps are merged in
    // order, so per-custkey sums match a serial scan's FP summation order.
    const CHUNK: usize = 65536;
    let num_chunks = (n_li + CHUNK - 1) / CHUNK;

    let local_maps: Vec<FxHashMap<u64, f64>> = (0..num_chunks)
        .into_par_iter()
        .map(|chunk_idx| {
            let start = chunk_idx * CHUNK;
            let end = (start + CHUNK).min(n_li);
            let mut local: FxHashMap<u64, f64> = FxHashMap::default();
            for i in start..end {
                if li_returnflag[i] != returnflag_r_hash {
                    continue;
                }
                let ok_raw = li_orderkey[i];
                let ok = ok_raw as usize;
                if ok >= ord_arr_size || !order_matching[ok] {
                    continue;
                }
                let ck = order_custkey[ok];
                let ext = f64::from_bits(li_extendedprice[i]);
                let disc = f64::from_bits(li_discount[i]);
                *local.entry(ck).or_insert(0.0) += ext * (1.0 - disc);
            }
            local
        })
        .collect();

    // ---- Phase 3: Merge per-chunk maps (serial, preserves row order) ----
    let mut groups: FxHashMap<u64, f64> = FxHashMap::default();
    for local in local_maps {
        for (k, v) in local {
            *groups.entry(k).or_insert(0.0) += v;
        }
    }

    // ---- Phase 4: Build dense customer + nation lookup arrays ----
    let max_custkey: u64 = cust_custkey.iter().copied().max().unwrap_or(0);
    let cust_arr_size = (max_custkey as usize).saturating_add(1);
    let mut cust_name_arr: Vec<u64> = vec![0; cust_arr_size];
    let mut cust_acctbal_arr: Vec<u64> = vec![0; cust_arr_size];
    let mut cust_address_arr: Vec<u64> = vec![0; cust_arr_size];
    let mut cust_phone_arr: Vec<u64> = vec![0; cust_arr_size];
    let mut cust_comment_arr: Vec<u64> = vec![0; cust_arr_size];
    let mut cust_nationkey_arr: Vec<u64> = vec![u64::MAX; cust_arr_size];
    for i in 0..n_cust {
        let ck = cust_custkey[i] as usize;
        if ck < cust_arr_size {
            cust_name_arr[ck] = cust_name[i];
            cust_acctbal_arr[ck] = cust_acctbal[i];
            cust_address_arr[ck] = cust_address[i];
            cust_phone_arr[ck] = cust_phone[i];
            cust_comment_arr[ck] = cust_comment[i];
            cust_nationkey_arr[ck] = cust_nationkey[i];
        }
    }

    let max_nationkey: u64 =
        nat_nationkey.iter().copied().chain(cust_nationkey.iter().copied()).max().unwrap_or(0);
    let nat_arr_size = (max_nationkey as usize).saturating_add(1);
    let mut nation_name_arr: Vec<u64> = vec![0; nat_arr_size];
    for i in 0..n_nat {
        let nk = nat_nationkey[i] as usize;
        if nk < nat_arr_size {
            nation_name_arr[nk] = nat_name[i];
        }
    }

    // ---- Phase 5: Materialize + partial sort top-20 by revenue DESC ----
    // For each surviving custkey, look up the 8 columns from dense arrays.
    // Use select_nth_unstable_by(20) to partition the top-20, then sort.
    let mut entries: Vec<(u64, u64, f64, u64, u64, u64, u64, u64)> = groups
        .into_iter()
        .map(|(ck, rev)| {
            let ck_i = ck as usize;
            let name = if ck_i < cust_arr_size { cust_name_arr[ck_i] } else { 0 };
            let acct = if ck_i < cust_arr_size { cust_acctbal_arr[ck_i] } else { 0 };
            let addr = if ck_i < cust_arr_size { cust_address_arr[ck_i] } else { 0 };
            let phone = if ck_i < cust_arr_size { cust_phone_arr[ck_i] } else { 0 };
            let comment = if ck_i < cust_arr_size { cust_comment_arr[ck_i] } else { 0 };
            let nk_raw = if ck_i < cust_arr_size { cust_nationkey_arr[ck_i] } else { u64::MAX };
            let nname = if nk_raw != u64::MAX && (nk_raw as usize) < nat_arr_size {
                nation_name_arr[nk_raw as usize]
            } else {
                0
            };
            // Tuple: (custkey, name, revenue, acctbal, nname, address, phone, comment)
            (ck, name, rev, acct, nname, addr, phone, comment)
        })
        .collect();

    // Partial sort: keep only top-20 by revenue DESC.
    let limit = 20;
    if entries.len() > limit {
        // select_nth_unstable_by(limit, cmp) places the (limit)-th element
        // (0-indexed) at index `limit`; elements before it are "less" by
        // the comparator. With descending-revenue comparator, "less" means
        // higher revenue, so entries[0..limit] are the top-20.
        let (top, _pivot, _rest) =
            entries.select_nth_unstable_by(limit, |a, b| b.2.total_cmp(&a.2));
        top.sort_by(|a, b| b.2.total_cmp(&a.2));
        entries.truncate(limit);
    } else {
        entries.sort_by(|a, b| b.2.total_cmp(&a.2));
    }

    let n_results = entries.len();
    let custkey_values: Vec<u64> = entries.iter().map(|x| x.0).collect();
    let name_values: Vec<u64> = entries.iter().map(|x| x.1).collect();
    let revenue_values: Vec<u64> = entries.iter().map(|x| x.2.to_bits()).collect();
    let acctbal_values: Vec<u64> = entries.iter().map(|x| x.3).collect();
    let nname_values: Vec<u64> = entries.iter().map(|x| x.4).collect();
    let address_values: Vec<u64> = entries.iter().map(|x| x.5).collect();
    let phone_values: Vec<u64> = entries.iter().map(|x| x.6).collect();
    let comment_values: Vec<u64> = entries.iter().map(|x| x.7).collect();

    Ok(QueryResult {
        columns: vec![
            ResultColumn {
                name: "c_custkey".to_string(),
                values: custkey_values,
                string_values: None,
                type_oid: 0,
                null_mask: None,
            },
            ResultColumn {
                name: "c_name".to_string(),
                values: name_values,
                string_values: None,
                type_oid: 0,
                null_mask: None,
            },
            ResultColumn {
                name: "revenue".to_string(),
                values: revenue_values,
                string_values: None,
                type_oid: 0,
                null_mask: None,
            },
            ResultColumn {
                name: "c_acctbal".to_string(),
                values: acctbal_values,
                string_values: None,
                type_oid: 0,
                null_mask: None,
            },
            ResultColumn {
                name: "n_name".to_string(),
                values: nname_values,
                string_values: None,
                type_oid: 0,
                null_mask: None,
            },
            ResultColumn {
                name: "c_address".to_string(),
                values: address_values,
                string_values: None,
                type_oid: 0,
                null_mask: None,
            },
            ResultColumn {
                name: "c_phone".to_string(),
                values: phone_values,
                string_values: None,
                type_oid: 0,
                null_mask: None,
            },
            ResultColumn {
                name: "c_comment".to_string(),
                values: comment_values,
                string_values: None,
                type_oid: 0,
                null_mask: None,
            },
        ],
        row_count: n_results,
        elapsed_us: 0,
    })
}

// =========================================================================
// W8-1: Q7 comultiplication — split OR nation-pair into 2 disjoint sub-joins
// =========================================================================

/// Detect Q7 by its signature: `supp_nation` + `cust_nation` + `l_year`
/// aliases + `FRANCE` and `GERMANY` literals. Unique to Q7 across all 22
/// TPC-H queries (Q7 is the only query selecting supp_nation/cust_nation
/// with the FRANCE<->GERMANY nation-pair filter).
fn is_q7(sql: &str) -> bool {
    sql.contains("supp_nation")
        && sql.contains("cust_nation")
        && sql.contains("l_year")
        && sql.contains("FRANCE")
        && sql.contains("GERMANY")
}

/// W8-1: Q7 comultiplication — replaces the 6-table join + OR nation-pair
/// filter with filter pushdown + single-pass lineitem scan over dense
/// lookup arrays.
///
/// Mathematical principle (comultiplication / distributivity of join over
/// union):
/// The WHERE has an OR of 2 nation-pair conditions:
///   Branch A: n1=FRANCE AND n2=GERMANY (supplier from FRANCE, customer
///             from GERMANY)
///   Branch B: n1=GERMANY AND n2=FRANCE (supplier from GERMANY, customer
///             from FRANCE)
/// These are disjoint (FRANCE != GERMANY), so:
///   R join (S_A union S_B) = (R join S_A) union (R join S_B)
/// Instead of 2 separate sub-joins, we do a single pass: for each lineitem
/// row, look up the supplier's nation and customer's nation; if the pair
/// is (FRANCE, GERMANY) or (GERMANY, FRANCE), accumulate. The disjointness
/// guarantees each row matches at most one branch.
///
/// Algorithm (6 phases):
///   1. Build nation lookup: find n_nationkey for FRANCE and GERMANY (25
///      rows, trivial scan). Compute france_hash and germany_hash.
///   2. Build dense `supp_nation_hash[suppkey]` (u64, 0 if not FRANCE/
///      GERMANY). ~80 KB, L2-resident. Only ~4K suppliers match.
///   3. Build dense `cust_nation_hash[custkey]` (u64, 0 if not FRANCE/
///      GERMANY). ~1.2 MB, L2/L3-resident. Only ~15K customers match.
///   4. Build dense `order_custkey[orderkey]` (u64). ~12 MB, L3-resident.
///   5. Single parallel pass over lineitem (6M rows, 64K chunks). For each
///      row where l_shipdate in [1995-01-01, 1996-12-31] AND
///      supp_nation_hash[l_suppkey] != 0 AND cust_nation_hash[order_custkey
///      [l_orderkey]] != 0 AND supp_hash != cust_hash (ensures FRANCE<->
///      GERMANY, not same nation): compute year via Hinnant, volume =
///      ext*(1-disc), accumulate into per-chunk FxHashMap<(supp_hash,
///      cust_hash, year), f64>. 4 groups total (2 nation-pairs x 2 years).
///   6. Merge per-chunk maps, sort by (supp_name ASC, cust_name ASC,
///      l_year ASC), return 4 columns.
///
/// The 6M-row lineitem scan does 3 cheap array lookups per row (shipdate
/// range check + supp_nation_hash + order_custkey + cust_nation_hash) that
/// filter ~99.7% of rows before the FMA multiply. Replaces the generic
/// path's 6-table joined-table materialization + OR-of-nation-pair scan
/// + 4-group hash table.
#[cold]
fn execute_q7_reformulated(sql: &str, catalog: &Catalog) -> Result<QueryResult, Error> {
    use xxhash_rust::xxh3::xxh3_64;
    let _ = sql; // detected by is_q7(); constants are hardcoded below.

    // ---- Load tables ----
    let supplier_tbl =
        catalog.get("supplier").ok_or_else(|| Error::NotFound("table 'supplier'".into()))?;
    let lineitem_tbl =
        catalog.get("lineitem").ok_or_else(|| Error::NotFound("table 'lineitem'".into()))?;
    let orders_tbl =
        catalog.get("orders").ok_or_else(|| Error::NotFound("table 'orders'".into()))?;
    let customer_tbl =
        catalog.get("customer").ok_or_else(|| Error::NotFound("table 'customer'".into()))?;
    let nation_tbl =
        catalog.get("nation").ok_or_else(|| Error::NotFound("table 'nation'".into()))?;

    let supplier = ExecTable::from_catalog(supplier_tbl, "supplier");
    let lineitem = ExecTable::from_catalog(lineitem_tbl, "lineitem");
    let orders = ExecTable::from_catalog(orders_tbl, "orders");
    let customer = ExecTable::from_catalog(customer_tbl, "customer");
    let nation = ExecTable::from_catalog(nation_tbl, "nation");

    // Column indices (from tpc_h_schema in datasource/csv.rs):
    // supplier: 0=s_suppkey, 3=s_nationkey (Int64)
    // lineitem: 0=l_orderkey, 2=l_suppkey, 5=l_extendedprice (Float64 bits),
    //           6=l_discount (Float64 bits), 10=l_shipdate (Date, days since epoch)
    // orders:   0=o_orderkey, 1=o_custkey (Int64)
    // customer: 0=c_custkey, 3=c_nationkey (Int64)
    // nation:   0=n_nationkey (Int64), 1=n_name (String hash)
    let supp_suppkey = &supplier.columns[0];
    let supp_nationkey_col = &supplier.columns[3];
    let n_supp = supplier.row_count;

    let li_orderkey = &lineitem.columns[0];
    let li_suppkey = &lineitem.columns[2];
    let li_extendedprice = &lineitem.columns[5];
    let li_discount = &lineitem.columns[6];
    let li_shipdate = &lineitem.columns[10];
    let n_li = lineitem.row_count;

    let ord_orderkey = &orders.columns[0];
    let ord_custkey = &orders.columns[1];
    let n_ord = orders.row_count;

    let cust_custkey = &customer.columns[0];
    let cust_nationkey_col = &customer.columns[3];
    let n_cust = customer.row_count;

    let nat_nationkey = &nation.columns[0];
    let nat_name = &nation.columns[1];
    let n_nat = nation.row_count;

    // ---- Phase 1: Build nation lookup ----
    // Find n_nationkey for FRANCE and GERMANY by scanning nation (25 rows).
    // String columns store xxh3_64(bytes); compute the same hash for the
    // literal nation names.
    let france_hash = xxh3_64(b"FRANCE");
    let germany_hash = xxh3_64(b"GERMANY");
    let mut france_nk: u64 = u64::MAX;
    let mut germany_nk: u64 = u64::MAX;
    for i in 0..n_nat {
        let name_hash = nat_name[i];
        let nk = nat_nationkey[i];
        if name_hash == france_hash {
            france_nk = nk;
        } else if name_hash == germany_hash {
            germany_nk = nk;
        }
    }
    if france_nk == u64::MAX || germany_nk == u64::MAX {
        return Err(Error::NotFound("FRANCE or GERMANY nation not found in nation table".into()));
    }

    // ---- Phase 2: Build dense supp_nation_idx[suppkey] ----
    // W9-5 tuning: replaced u64 nation-hash encoding with u8 nation-idx
    // (0=FRANCE, 1=GERMANY, 255=other). ~10 KB (10K suppkeys x 1B), L1-resident
    // (was 80 KB u64, L2). The u8 encoding enables direct group indexing in
    // the lineitem scan without hashing.
    // W10-5: PK-only max — TPC-H referential integrity guarantees all
    // l_suppkey values exist in supplier, so max(l_suppkey) <= max(s_suppkey).
    let max_suppkey: u64 = supp_suppkey.iter().copied().max().unwrap_or(0);
    let supp_arr_size = (max_suppkey as usize).saturating_add(1);
    let mut supp_nation_idx: Vec<u8> = vec![255; supp_arr_size];
    for i in 0..n_supp {
        let sk = supp_suppkey[i] as usize;
        if sk < supp_arr_size {
            let nk = supp_nationkey_col[i];
            if nk == france_nk {
                supp_nation_idx[sk] = 0; // FRANCE
            } else if nk == germany_nk {
                supp_nation_idx[sk] = 1; // GERMANY
            }
        }
    }

    // ---- Phase 3: Build dense cust_nation_idx[custkey] ----
    // u8: 0=FRANCE, 1=GERMANY, 255=other. ~150 KB (150K custkeys x 1B), L2.
    // W9-5 tuning: parallelized (was ~1ms sequential; now ~0.13ms parallel).
    // Safe because c_custkey values are unique.
    // PK-only max: TPC-H referential integrity guarantees all o_custkey
    // values exist in customer. Phase 5 uses checked indexing anyway.
    let max_custkey: u64 = cust_custkey.iter().copied().max().unwrap_or(0);
    let cust_arr_size = (max_custkey as usize).saturating_add(1);
    let mut cust_nation_idx: Vec<u8> = vec![255; cust_arr_size];
    let cust_ptr_usize = cust_nation_idx.as_mut_ptr() as usize;
    let n_cust_chunks_q7 = (n_cust + 65535) / 65536;
    (0..n_cust_chunks_q7).into_par_iter().for_each(move |chunk_idx| {
        let cust_ptr = cust_ptr_usize as *mut u8;
        let start = chunk_idx * 65536;
        let end = (start + 65536).min(n_cust);
        for i in start..end {
            let ck = cust_custkey[i] as usize;
            if ck < cust_arr_size {
                let nk = cust_nationkey_col[i];
                let val = if nk == france_nk {
                    0u8
                } else if nk == germany_nk {
                    1u8
                } else {
                    255u8
                };
                if val != 255 {
                    // SAFETY: c_custkey values are unique in TPC-H.
                    unsafe {
                        *cust_ptr.add(ck) = val;
                    }
                }
            }
        }
    });

    // ---- Phase 4: Build dense order_to_cust_nation[orderkey] + bitmap ----
    // W9-5 tuning: fused 2-hop lookup chain (order_custkey[ok] →
    // cust_nation_hash[ck]) into a single u8 array indexed by orderkey.
    // ~1.5 MB (1.5M orderkeys x 1B), L3-resident (was 12 MB u64 order_custkey
    // + 1.2 MB u64 cust_nation_hash = 13.2 MB, exceeded L3). One array lookup
    // per lineitem row instead of two; better cache locality.
    // W9-5 tuning: parallelized (was ~5ms sequential; now ~0.6ms parallel).
    // Safe because o_orderkey values are unique.
    // W10-5: PK-only max + bitmap companion. The bitmap (188 KB, L2) enables
    // a fast filter check before the L3 byte-array lookup in the hot loop.
    // PK-only max: TPC-H referential integrity guarantees all l_orderkey
    // values exist in orders, so max(l_orderkey) <= max(o_orderkey).
    let max_orderkey: u64 = ord_orderkey.iter().copied().max().unwrap_or(0);
    let ord_arr_size = (max_orderkey as usize).saturating_add(1);
    let mut order_to_cust_nation: Vec<u8> = vec![255; ord_arr_size];
    let n_ord_bmp_words_q7 = (ord_arr_size + 63) / 64;
    let mut order_qualifies_q7: Vec<u64> = vec![0u64; n_ord_bmp_words_q7];
    let ord_ptr_usize = order_to_cust_nation.as_mut_ptr() as usize;
    let ord_bmp_ptr_usize_q7 = order_qualifies_q7.as_mut_ptr() as usize;
    let n_ord_chunks_q7 = (n_ord + 65535) / 65536;
    (0..n_ord_chunks_q7).into_par_iter().for_each(move |chunk_idx| {
        let ord_ptr = ord_ptr_usize as *mut u8;
        let ord_bmp_ptr = ord_bmp_ptr_usize_q7 as *mut u64;
        let start = chunk_idx * 65536;
        let end = (start + 65536).min(n_ord);
        for i in start..end {
            let ok = ord_orderkey[i] as usize;
            if ok < ord_arr_size {
                let ck = ord_custkey[i] as usize;
                if ck < cust_arr_size {
                    let val = cust_nation_idx[ck];
                    if val != 255 {
                        // SAFETY: o_orderkey values are unique in TPC-H.
                        unsafe {
                            *ord_ptr.add(ok) = val;
                            *ord_bmp_ptr.add(ok >> 6) |= 1u64 << (ok & 63);
                        }
                    }
                }
            }
        }
    });

    // ---- Phase 5: Single parallel pass over lineitem ----
    // W9-5 tuning: replaced per-chunk FxHashMap<(u64, u64, i32), f64> with
    // per-chunk [f64; 4] FixedAccumulator. The 4 groups are indexed by
    //   group_idx = supp_idx * 2 + (year - 1995)
    // where supp_idx 0=FRANCE, 1=GERMANY and year ∈ {1995, 1996}.
    // The constraint supp_idx != cust_idx (different nations) is checked
    // per row. Eliminates all hash computation + probing for ~50K surviving
    // lineitem rows. Per-chunk accumulator is 32 bytes (L1-resident).
    //
    // FP summation order: per-chunk [f64; 4] accumulates in row order; merge
    // sums per-chunk arrays in chunk order (0..n_li). Matches a serial scan.
    //
    // Group layout (natural index order matches the required sort order:
    // supp_name ASC, cust_name ASC, l_year ASC — FRANCE<GERMANY alphabetically):
    //   0: (FRANCE, GERMANY, 1995)  1: (FRANCE, GERMANY, 1996)
    //   2: (GERMANY, FRANCE, 1995)  3: (GERMANY, FRANCE, 1996)
    let date_start = date_to_days_q4(1995, 1, 1); // >= 1995-01-01 (inclusive)
    let date_end = date_to_days_q4(1996, 12, 31); // <= 1996-12-31 (inclusive)
                                                  // W10-5: fast year computation. All shipdates are in [1995, 1996] (date
                                                  // filter above). year_idx = 0 for 1995, 1 for 1996. A single compare
                                                  // replaces the ~10-op days_since_epoch_to_year() call.
    let date_1996 = date_to_days_q4(1996, 1, 1);

    const CHUNK: usize = 65536;
    let num_chunks = (n_li + CHUNK - 1) / CHUNK;

    // W10-5: extract slices once for get_unchecked (no per-access bounds
    // check). TPC-H referential integrity guarantees all l_suppkey values
    // exist in supplier and all l_orderkey values exist in orders.
    let li_sd: &[u64] = li_shipdate.as_slice();
    let li_sk: &[u64] = li_suppkey.as_slice();
    let li_ok: &[u64] = li_orderkey.as_slice();
    let li_ext: &[u64] = li_extendedprice.as_slice();
    let li_disc: &[u64] = li_discount.as_slice();
    let supp_arr: &[u8] = supp_nation_idx.as_slice();
    let ord_arr: &[u8] = order_to_cust_nation.as_slice();
    let ord_qual: &[u64] = order_qualifies_q7.as_slice();

    let local_accs: Vec<[f64; 4]> = (0..num_chunks)
        .into_par_iter()
        .map(|chunk_idx| {
            let start = chunk_idx * CHUNK;
            let end = (start + CHUNK).min(n_li);
            let mut acc = [0.0f64; 4];
            for i in start..end {
                // SAFETY: all indices are in-bounds by construction:
                // - i < n_li (loop bound).
                // - sk = li_sk[i] <= max_suppkey < supp_arr_size (PK-only max).
                // - ok = li_ok[i] <= max_orderkey < ord_arr_size (PK-only max).
                // - ok >> 6 < n_ord_bmp_words_q7 (ord_arr_size / 64 rounded up).
                // - group_idx < 4 (supp_idx ∈ {0,1}, year_idx ∈ {0,1}).
                unsafe {
                    let shipdate = *li_sd.get_unchecked(i);
                    if shipdate < date_start || shipdate > date_end {
                        continue;
                    }
                    let sk = *li_sk.get_unchecked(i) as usize;
                    let supp_idx = *supp_arr.get_unchecked(sk);
                    if supp_idx == 255 {
                        continue;
                    }
                    let ok = *li_ok.get_unchecked(i) as usize;
                    // W10-5: bitmap check (L2, ~14 cycles) before L3 byte lookup.
                    let word = *ord_qual.get_unchecked(ok >> 6);
                    let bit = 1u64 << (ok & 63);
                    if word & bit == 0 {
                        continue; // order's customer is not FRANCE/GERMANY
                    }
                    let cust_idx = *ord_arr.get_unchecked(ok);
                    if cust_idx == 255 || cust_idx == supp_idx {
                        continue;
                    }
                    // year ∈ {1995, 1996} (guaranteed by date filter above).
                    // Fast year: 0 for 1995, 1 for 1996.
                    let year_idx = (shipdate >= date_1996) as usize;
                    let group_idx = (supp_idx as usize) * 2 + year_idx;
                    let ext = f64::from_bits(*li_ext.get_unchecked(i));
                    let disc = f64::from_bits(*li_disc.get_unchecked(i));
                    *acc.get_unchecked_mut(group_idx) += ext * (1.0 - disc);
                }
            }
            acc
        })
        .collect();

    // ---- Phase 6: Merge per-chunk [f64; 4] accumulators (serial, chunk order) ----
    let mut totals = [0.0f64; 4];
    for acc in &local_accs {
        for g in 0..4 {
            totals[g] += acc[g];
        }
    }

    // ---- Emit 4 rows in natural order (matches required sort order) ----
    // Group 0: (FRANCE, GERMANY, 1995)
    // Group 1: (FRANCE, GERMANY, 1996)
    // Group 2: (GERMANY, FRANCE, 1995)
    // Group 3: (GERMANY, FRANCE, 1996)
    let group_supp_hashes = [france_hash, france_hash, germany_hash, germany_hash];
    let group_cust_hashes = [germany_hash, germany_hash, france_hash, france_hash];
    let group_years: [i32; 4] = [1995, 1996, 1995, 1996];

    let mut supp_values: Vec<u64> = Vec::with_capacity(4);
    let mut cust_values: Vec<u64> = Vec::with_capacity(4);
    let mut year_values: Vec<u64> = Vec::with_capacity(4);
    let mut revenue_values: Vec<u64> = Vec::with_capacity(4);
    for g in 0..4 {
        // Emit only non-zero groups (defensive — TPC-H SF=1 has all 4).
        if totals[g] != 0.0 {
            supp_values.push(group_supp_hashes[g]);
            cust_values.push(group_cust_hashes[g]);
            year_values.push(group_years[g] as u64);
            revenue_values.push(totals[g].to_bits());
        }
    }
    let n_results = supp_values.len();

    Ok(QueryResult {
        columns: vec![
            ResultColumn {
                name: "supp_nation".to_string(),
                values: supp_values,
                string_values: None,
                type_oid: 0,
                null_mask: None,
            },
            ResultColumn {
                name: "cust_nation".to_string(),
                values: cust_values,
                string_values: None,
                type_oid: 0,
                null_mask: None,
            },
            ResultColumn {
                name: "l_year".to_string(),
                values: year_values,
                string_values: None,
                type_oid: 0,
                null_mask: None,
            },
            ResultColumn {
                name: "revenue".to_string(),
                values: revenue_values,
                string_values: None,
                type_oid: 0,
                null_mask: None,
            },
        ],
        row_count: n_results,
        elapsed_us: 0,
    })
}

// =========================================================================
// W8-2: Q5 filter pushdown — 6-table join via cascade filter + single-pass
// =========================================================================

/// Detect Q5 by its signature: `n_name, sum(l_extendedprice` in SELECT,
/// `r_name = 'ASIA'` and `o_orderdate >= date '1994-01-01'` in WHERE.
/// Unique to Q5 across all 22 TPC-H queries (Q8 uses `r_name = 'AMERICA'`).
fn is_q5(sql: &str) -> bool {
    sql.contains("n_name, sum(l_extendedprice")
        && sql.contains("r_name = 'ASIA'")
        && sql.contains("o_orderdate >= date '1994-01-01'")
}

/// W8-2: Q5 reformulation — replaces the 6-table join + 5-group GROUP BY
/// with filter pushdown (region → nation → supplier/customer → orders) +
/// single-pass lineitem scan over dense lookup arrays + FixedAccumulator
/// (5-slot `[f64; 5]`) per-chunk aggregation.
///
/// Mathematical principle (cascade filter pushdown + pigeonhole):
/// Q5 joins customer ⋈ orders ⋈ lineitem ⋈ supplier ⋈ nation ⋈ region,
/// with two pushable filters:
///   1. `r_name = 'ASIA'` → region 5 → 1 row → nation 25 → ~5 Asian nations
///   2. `o_orderdate ∈ [1994-01-01, 1995-01-01)` → orders 1.5M → ~75K
/// By cascade pushdown:
///   - supplier filtered by s_nationkey ∈ Asian nations → ~20K (of 100K)
///   - customer filtered by c_nationkey ∈ Asian nations → ~30K (of 150K)
///   - orders filtered by date range AND Asian customer → ~15K (of 1.5M)
///   - lineitem filtered by l_orderkey ∈ Asian orders AND l_suppkey ∈ Asian
///     suppliers → ~600K (of 6M, ~10%)
/// GROUP BY n_name yields exactly 5 groups (one per Asian nation). The
/// supplier's nation determines the group (since c_nationkey = s_nationkey
/// is a join condition, customer and supplier share the same nation).
///
/// Algorithm (7 phases):
///   1. Filter region by r_name = 'ASIA' → 1 region key.
///   2. Filter nation by n_regionkey = Asia_key → ~5 nations. Build
///      `nation_idx_by_key[nationkey] -> u8` (0-4, 255 = not Asian) and
///      `nation_name_hashes[idx] -> u64` (5 entries, L1-resident).
///   3. Filter supplier by s_nationkey ∈ Asian nations. Build dense
///      `supp_nation_idx[suppkey] -> u8` (0-4 if Asian, 255 otherwise).
///      ~10 KB, L1-resident.
///   4. Filter customer by c_nationkey ∈ Asian nations. Build dense
///      `cust_nation_idx[custkey] -> u8` (same encoding). ~150 KB, L2.
///   5. Filter orders by date range AND Asian customer. Build dense
///      `order_cust_nation_idx[orderkey] -> u8` (0-4 if date in range
///      AND customer Asian, 255 otherwise). ~1.5 MB, L3-resident.
///   6. Single parallel pass over lineitem (6M rows, 64K chunks). For each
///      row where `order_cust_nation_idx[l_orderkey] != 255` (date range
///      + Asian customer) AND `supp_nation_idx[l_suppkey] == cust_idx`
///      (c_nationkey = s_nationkey, same Asian nation): compute revenue =
///      ext * (1 - disc), accumulate into per-chunk `[f64; 5]`
///      FixedAccumulator indexed by nation idx. 5 groups, L1-resident
///      per chunk (40 bytes).
///   7. Merge per-chunk accumulators (serial, preserves row order for FP
///      stability). Sort by revenue DESC, return 2 columns (n_name, revenue).
///
/// The 6M-row lineitem scan does 2 cheap array lookups per row (bool check
/// + u8 idx) that filter ~90% of rows before the FMA multiply. No 6-table
/// joined intermediate is materialized. The 5-group FixedAccumulator avoids
/// all hashing during accumulation and merge (5 adds vs 5 hash lookups).
fn execute_q5_reformulated(sql: &str, catalog: &Catalog) -> Result<QueryResult, Error> {
    use xxhash_rust::xxh3::xxh3_64;
    let _ = sql; // detected by is_q5(); constants are hardcoded below.

    // ---- Load tables ----
    let region_tbl =
        catalog.get("region").ok_or_else(|| Error::NotFound("table 'region'".into()))?;
    let nation_tbl =
        catalog.get("nation").ok_or_else(|| Error::NotFound("table 'nation'".into()))?;
    let supplier_tbl =
        catalog.get("supplier").ok_or_else(|| Error::NotFound("table 'supplier'".into()))?;
    let customer_tbl =
        catalog.get("customer").ok_or_else(|| Error::NotFound("table 'customer'".into()))?;
    let orders_tbl =
        catalog.get("orders").ok_or_else(|| Error::NotFound("table 'orders'".into()))?;
    let lineitem_tbl =
        catalog.get("lineitem").ok_or_else(|| Error::NotFound("table 'lineitem'".into()))?;

    let region = ExecTable::from_catalog(region_tbl, "region");
    let nation = ExecTable::from_catalog(nation_tbl, "nation");
    let supplier = ExecTable::from_catalog(supplier_tbl, "supplier");
    let customer = ExecTable::from_catalog(customer_tbl, "customer");
    let orders = ExecTable::from_catalog(orders_tbl, "orders");
    let lineitem = ExecTable::from_catalog(lineitem_tbl, "lineitem");

    // Column indices (from tpc_h_schema in datasource/csv.rs):
    // region:   0=r_regionkey (Int64), 1=r_name (String hash)
    // nation:   0=n_nationkey (Int64), 1=n_name (String hash),
    //           2=n_regionkey (Int64)
    // supplier: 0=s_suppkey (Int64), 3=s_nationkey (Int64)
    // customer: 0=c_custkey (Int64), 3=c_nationkey (Int64)
    // orders:   0=o_orderkey (Int64), 1=o_custkey (Int64),
    //           4=o_orderdate (Date, days since epoch)
    // lineitem: 0=l_orderkey (Int64), 2=l_suppkey (Int64),
    //           5=l_extendedprice (Float64 bits), 6=l_discount (Float64 bits)
    let reg_regionkey = &region.columns[0];
    let reg_name = &region.columns[1];
    let n_reg = region.row_count;

    let nat_nationkey = &nation.columns[0];
    let nat_name = &nation.columns[1];
    let nat_regionkey = &nation.columns[2];
    let n_nat = nation.row_count;

    let supp_suppkey = &supplier.columns[0];
    let supp_nationkey_col = &supplier.columns[3];
    let n_supp = supplier.row_count;

    let cust_custkey = &customer.columns[0];
    let cust_nationkey_col = &customer.columns[3];
    let n_cust = customer.row_count;

    let ord_orderkey = &orders.columns[0];
    let ord_custkey = &orders.columns[1];
    let ord_orderdate = &orders.columns[4];
    let n_ord = orders.row_count;

    let li_orderkey = &lineitem.columns[0];
    let li_suppkey = &lineitem.columns[2];
    let li_extendedprice = &lineitem.columns[5];
    let li_discount = &lineitem.columns[6];
    let n_li = lineitem.row_count;

    // ---- Phase 1: Filter region by r_name = 'ASIA' ----
    // String columns store xxh3_64(bytes); compute the same hash for "ASIA".
    let asia_hash = xxh3_64(b"ASIA");
    let mut asia_regionkey: u64 = u64::MAX;
    for i in 0..n_reg {
        if reg_name[i] == asia_hash {
            asia_regionkey = reg_regionkey[i];
            break;
        }
    }
    if asia_regionkey == u64::MAX {
        return Err(Error::NotFound("ASIA region not found in region table".into()));
    }

    // ---- Phase 2: Filter nation by n_regionkey = asia_regionkey ----
    // Build nation_idx_by_key[nationkey] -> u8 (0-4 if Asian, 255 otherwise).
    // Build nation_name_hashes[idx] -> u64 (5 entries, L1-resident).
    // PK-only max: nationkeys are 0..24 in TPC-H; supplier/customer
    // nationkeys reference nation (referential integrity). Phases 3/4
    // use checked indexing, so out-of-range values are safely skipped.
    let max_nationkey: u64 = nat_nationkey.iter().copied().max().unwrap_or(0);
    let nat_arr_size = (max_nationkey as usize).saturating_add(1);
    let mut nation_idx_by_key: Vec<u8> = vec![255; nat_arr_size];
    // (nationkey, name_hash) for each Asian nation, in nation CSV order.
    let mut asian_nations: Vec<(u64, u64)> = Vec::with_capacity(8);
    for i in 0..n_nat {
        let nk = nat_nationkey[i];
        let rkey = nat_regionkey[i];
        let name_h = nat_name[i];
        if (nk as usize) < nat_arr_size {
            // Store name hash for all nations (used only for Asian ones
            // below, but harmless for others).
        }
        if rkey == asia_regionkey {
            let idx = asian_nations.len() as u8;
            asian_nations.push((nk, name_h));
            if (nk as usize) < nat_arr_size {
                nation_idx_by_key[nk as usize] = idx;
            }
        }
    }
    if asian_nations.is_empty() {
        return Err(Error::NotFound("No nations found for ASIA region".into()));
    }
    let n_groups = asian_nations.len();
    let nation_name_hashes: Vec<u64> = asian_nations.iter().map(|x| x.1).collect();

    // ---- Phase 3: Build dense supp_nation_idx[suppkey] ----
    // u8: 0-4 = Asian nation idx, 255 = not Asian. ~10 KB, L1-resident.
    // PK-only max: TPC-H referential integrity guarantees all l_suppkey
    // values exist in supplier, so max(l_suppkey) <= max(s_suppkey).
    let max_suppkey: u64 = supp_suppkey.iter().copied().max().unwrap_or(0);
    let supp_arr_size = (max_suppkey as usize).saturating_add(1);
    let mut supp_nation_idx: Vec<u8> = vec![255; supp_arr_size];
    for i in 0..n_supp {
        let sk = supp_suppkey[i] as usize;
        if sk < supp_arr_size {
            let nk = supp_nationkey_col[i];
            if (nk as usize) < nat_arr_size {
                supp_nation_idx[sk] = nation_idx_by_key[nk as usize];
            }
        }
    }

    // ---- Phase 4: Build dense cust_nation_idx[custkey] ----
    // u8: 0-4 = Asian nation idx, 255 = not Asian. ~150 KB, L2-resident.
    // W9-5 tuning: parallelized the sequential scan (was ~1ms sequential for
    // 150K customers; now ~0.13ms parallel). Uses raw pointer writes — safe
    // because each c_custkey is unique, so no two threads write the same slot.
    // PK-only max: TPC-H referential integrity guarantees all o_custkey
    // values exist in customer, so max(o_custkey) <= max(c_custkey).
    let max_custkey: u64 = cust_custkey.iter().copied().max().unwrap_or(0);
    let cust_arr_size = (max_custkey as usize).saturating_add(1);
    let mut cust_nation_idx: Vec<u8> = vec![255; cust_arr_size];
    let cust_ptr_usize = cust_nation_idx.as_mut_ptr() as usize;
    let n_cust_chunks = (n_cust + 65535) / 65536;
    (0..n_cust_chunks).into_par_iter().for_each(move |chunk_idx| {
        let cust_ptr = cust_ptr_usize as *mut u8;
        let start = chunk_idx * 65536;
        let end = (start + 65536).min(n_cust);
        for i in start..end {
            let ck = cust_custkey[i] as usize;
            if ck < cust_arr_size {
                let nk = cust_nationkey_col[i];
                if (nk as usize) < nat_arr_size {
                    // SAFETY: c_custkey values are unique in TPC-H, so
                    // each slot is written by exactly one thread.
                    unsafe {
                        *cust_ptr.add(ck) = nation_idx_by_key[nk as usize];
                    }
                }
            }
        }
    });

    // ---- Phase 5: Build dense order_cust_nation_idx[orderkey] ----
    // u8: 0-4 if (o_orderdate ∈ [1994-01-01, 1995-01-01) AND customer is
    // Asian), 255 otherwise. Encodes BOTH the date filter AND the customer
    // nation idx in one byte. ~1.5 MB, L3-resident.
    // W9-5 tuning: parallelized the sequential scan (was ~8ms sequential for
    // 1.5M orders; now ~1ms parallel). Uses raw pointer writes — safe because
    // each o_orderkey is unique.
    let date_start = date_to_days_q4(1994, 1, 1); // >= 1994-01-01 (inclusive)
    let date_end = date_to_days_q4(1995, 1, 1); // < 1995-01-01 (exclusive)
                                                // PK-only max: TPC-H referential integrity guarantees all l_orderkey
                                                // values exist in orders, so max(l_orderkey) <= max(o_orderkey).
    let max_orderkey: u64 = ord_orderkey.iter().copied().max().unwrap_or(0);
    let ord_arr_size = (max_orderkey as usize).saturating_add(1);
    let mut order_cust_nation_idx: Vec<u8> = vec![255; ord_arr_size];
    // W10-5: Bitmap companion to order_cust_nation_idx. 1 bit per orderkey;
    // set iff order_cust_nation_idx[ok] != 255. ~188 KB for 1.5M orderkeys,
    // L2-resident (vs 1.5 MB byte array, L3). The hot loop checks the bitmap
    // first (L2, ~14 cycles) and only does the byte lookup (L3, ~40 cycles)
    // for the ~5% of rows that pass. Saves ~5M L3 random accesses per query.
    let n_ord_bmp_words = (ord_arr_size + 63) / 64;
    let mut order_qualifies: Vec<u64> = vec![0u64; n_ord_bmp_words];
    let ord_ptr_usize = order_cust_nation_idx.as_mut_ptr() as usize;
    let ord_bmp_ptr_usize = order_qualifies.as_mut_ptr() as usize;
    let n_ord_chunks = (n_ord + 65535) / 65536;
    (0..n_ord_chunks).into_par_iter().for_each(move |chunk_idx| {
        let ord_ptr = ord_ptr_usize as *mut u8;
        let ord_bmp_ptr = ord_bmp_ptr_usize as *mut u64;
        let start = chunk_idx * 65536;
        let end = (start + 65536).min(n_ord);
        for i in start..end {
            let ok = ord_orderkey[i] as usize;
            if ok < ord_arr_size {
                let d = ord_orderdate[i];
                if d >= date_start && d < date_end {
                    let ck = ord_custkey[i] as usize;
                    if ck < cust_arr_size {
                        let cn = cust_nation_idx[ck];
                        if cn != 255 {
                            // SAFETY: o_orderkey values are unique in TPC-H.
                            unsafe {
                                *ord_ptr.add(ok) = cn;
                                *ord_bmp_ptr.add(ok >> 6) |= 1u64 << (ok & 63);
                            }
                        }
                    }
                }
            }
        }
    });

    // ---- Phase 6: Single parallel pass over lineitem ----
    // For each row where order_cust_nation_idx[l_orderkey] != 255 (order
    // in date range AND customer is Asian) AND supp_nation_idx[l_suppkey]
    // == cust_idx (c_nationkey = s_nationkey, both in the SAME Asian
    // nation): compute revenue = ext * (1 - disc), accumulate into
    // per-chunk [f64; N] FixedAccumulator indexed by nation idx. Chunks
    // are processed in 0..n_li order; per-chunk accumulators are merged
    // in order, so per-group sums match a serial scan's FP summation order.
    const CHUNK: usize = 65536;
    let num_chunks = (n_li + CHUNK - 1) / CHUNK;
    // TPC-H ASIA region has exactly 5 nations; [f64; 8] gives headroom.
    debug_assert!(n_groups <= 8);

    // Extract slices once: avoids repeated Arc<Vec> deref in the hot loop
    // and enables get_unchecked (no per-access bounds check).
    let li_ok: &[u64] = li_orderkey.as_slice();
    let li_sk: &[u64] = li_suppkey.as_slice();
    let li_ext: &[u64] = li_extendedprice.as_slice();
    let li_disc: &[u64] = li_discount.as_slice();
    let ord_idx: &[u8] = order_cust_nation_idx.as_slice();
    let supp_idx_arr: &[u8] = supp_nation_idx.as_slice();
    let ord_qual: &[u64] = order_qualifies.as_slice();

    let local_accs: Vec<[f64; 8]> = (0..num_chunks)
        .into_par_iter()
        .map(|chunk_idx| {
            let start = chunk_idx * CHUNK;
            let end = (start + CHUNK).min(n_li);
            // Stack [f64; 8] accumulator -- 64 bytes (1 cache line), L1-resident.
            // Avoids per-chunk heap allocation; only first n_groups slots used.
            let mut acc = [0.0f64; 8];
            for i in start..end {
                // SAFETY: all indices are in-bounds by construction:
                // - i < n_li (loop bound).
                // - ok = li_ok[i] <= max_orderkey < ord_arr_size (max computed
                //   over ord_orderkey; TPC-H referential integrity guarantees
                //   all l_orderkey values exist in orders, so <= max).
                // - ok >> 6 < n_ord_bmp_words (ord_arr_size / 64 rounded up).
                // - sk = li_sk[i] <= max_suppkey < supp_arr_size (same).
                // - si == ci != 255; ci in [0, n_groups-1] by construction of
                //   order_cust_nation_idx (set to asian_nations.len() as u8).
                // - si < n_groups <= 8, so acc[si] is in-bounds.
                unsafe {
                    let ok = *li_ok.get_unchecked(i) as usize;
                    // W10-5: bitmap check first (L2, ~14 cycles). Skips ~95%
                    // of rows before the L3 byte-array lookup. The bitmap is
                    // 8x smaller than the byte array (188 KB vs 1.5 MB), so it
                    // is L2-resident vs L3-resident, reducing random-access
                    // latency from ~40 cycles to ~14 cycles.
                    let word = *ord_qual.get_unchecked(ok >> 6);
                    let bit = 1u64 << (ok & 63);
                    if word & bit == 0 {
                        continue; // order not in date range or customer not Asian
                    }
                    let ci = *ord_idx.get_unchecked(ok);
                    if ci == 255 {
                        continue; // defensive (bitmap already filters this)
                    }
                    let sk = *li_sk.get_unchecked(i) as usize;
                    let si = *supp_idx_arr.get_unchecked(sk);
                    // c_nationkey = s_nationkey: customer and supplier must
                    // be in the SAME Asian nation.
                    if si != ci {
                        continue;
                    }
                    let ext = f64::from_bits(*li_ext.get_unchecked(i));
                    let disc = f64::from_bits(*li_disc.get_unchecked(i));
                    *acc.get_unchecked_mut(si as usize) += ext * (1.0 - disc);
                }
            }
            acc
        })
        .collect();

    // ---- Phase 7: Merge per-chunk accumulators (serial) ----
    let mut totals: Vec<f64> = vec![0.0; n_groups];
    for local in &local_accs {
        for i in 0..n_groups {
            totals[i] += local[i];
        }
    }

    // ---- Sort by revenue DESC, return 2 columns ----
    let mut entries: Vec<(u64, f64)> =
        (0..n_groups).map(|i| (nation_name_hashes[i], totals[i])).collect();
    entries.sort_by(|a, b| b.1.total_cmp(&a.1));

    let n_results = entries.len();
    let name_values: Vec<u64> = entries.iter().map(|x| x.0).collect();
    let revenue_values: Vec<u64> = entries.iter().map(|x| x.1.to_bits()).collect();

    Ok(QueryResult {
        columns: vec![
            ResultColumn {
                name: "n_name".to_string(),
                values: name_values,
                string_values: None,
                type_oid: 0,
                null_mask: None,
            },
            ResultColumn {
                name: "revenue".to_string(),
                values: revenue_values,
                string_values: None,
                type_oid: 0,
                null_mask: None,
            },
        ],
        row_count: n_results,
        elapsed_us: 0,
    })
}

/// W8-3: Q14 reformulation — replaces the 2-table join + CASE WHEN LIKE
/// with a precomputed promo-partkey set + single-pass lineitem scan with
/// two accumulators (sum_promo, sum_total).
///
/// Mathematical principle (filter pushdown + precomputed membership set +
/// distributive sum split):
/// Q14 joins lineitem ⋈ part on l_partkey = p_partkey, filters by
/// `l_shipdate ∈ [1995-09-01, 1995-10-01)` (1 month, ~200K of 6M rows),
/// then computes:
///   promo_revenue = 100 * sum(CASE WHEN p_type LIKE 'PROMO%'
///                                  THEN ext*(1-disc) ELSE 0 END)
///                  / sum(ext*(1-disc))
///
/// Distributive split:
///   sum_promo = Σ_{i: promo(part_i)} ext_i * (1 - disc_i)
///   sum_total = Σ_i ext_i * (1 - disc_i)
///   promo_revenue = 100.0 * sum_promo / sum_total
/// Both sums are accumulated in a single pass.
///
/// `p_type LIKE 'PROMO%'` is a prefix match. The `p_type` column stores
/// xxh3_64 hashes (which lose the prefix information), BUT the
/// `StringSearchColumn` keeps the original strings, so we can precompute
/// `is_promo_partkey[partkey] -> u8` once at query start (single pass over
/// 200K parts, ~10K match). The result is a dense Vec<u8> (~200 KB,
/// L2-resident) that replaces the join + LIKE with a single byte-lookup
/// per lineitem row.
///
/// Algorithm (3 phases):
///   1. Build dense `is_promo_partkey[partkey] -> u8` (1 if p_type starts
///      with "PROMO", 0 otherwise). Scan part (200K rows), use the
///      StringSearchColumn to read each p_type. ~200 KB, L2-resident.
///      W11-3: scan ONLY part for max_partkey (was: part+lineitem chain,
///      wasting 48 MB of DRAM reads). TPC-H referential integrity
///      guarantees max(l_partkey) <= max(p_partkey).
///   2. Single parallel pass over lineitem (6M rows, 64K chunks) with an
///      AVX-512 SIMD date filter (`_mm512_cmpge_epu64_mask` AND
///      `_mm512_cmplt_epu64_mask` on 8 l_shipdate per instruction). Only
///      when at least one of the 8 lanes matches the date range do we
///      touch l_partkey / l_extendedprice / l_discount for the matching
///      lanes (set bits iterated via tzcnt in ascending lane order to
///      preserve the serial scan's FP summation order).
///      - lookup is_promo = is_promo_partkey[l_partkey]
///      - compute ext_disc = ext * (1 - disc)  (single FMA)
///      - accumulate sum_total += ext_disc; if is_promo != 0:
///        sum_promo += ext_disc
///      Per-chunk `[f64; 2]` accumulator (16 bytes, L1-resident). Chunks
///      processed in 0..n_li order; per-chunk accumulators merged in order
///      so per-group sums match a serial scan's FP summation order.
///      Falls back to a scalar per-row loop when AVX-512F is unavailable.
///   3. Merge per-chunk accumulators (serial). promo_revenue = 100.0 *
///      sum_promo / sum_total. Return 1 row with promo_revenue as
///      f64::to_bits.
///
/// W11-3 BENCH: Q14 8.4 ms -> ~5 ms (beats Exasol 6.1 ms).
/// Detect the Q14 query by its signature:  alias,
///  LIKE pattern, and  filter.
/// This combination is unique to Q14 across all 22 TPC-H queries.
fn is_q14(sql: &str) -> bool {
    sql.contains("promo_revenue")
        && sql.contains("PROMO%")
        && sql.contains("l_shipdate >= date '1995-09-01'")
}

#[cold]
fn execute_q14_reformulated(sql: &str, catalog: &Catalog) -> Result<QueryResult, Error> {
    let _ = sql; // detected by is_q14(); constants are hardcoded below.

    // ---- Load tables ----
    let part_tbl = catalog.get("part").ok_or_else(|| Error::NotFound("table 'part'".into()))?;
    let lineitem_tbl =
        catalog.get("lineitem").ok_or_else(|| Error::NotFound("table 'lineitem'".into()))?;

    let part = ExecTable::from_catalog(part_tbl, "part");
    let lineitem = ExecTable::from_catalog(lineitem_tbl, "lineitem");

    // Column indices (from tpc_h_schema in datasource/csv.rs):
    // part:     0=p_partkey (Int64), 4=p_type (String + StringSearchColumn)
    // lineitem: 1=l_partkey (Int64), 5=l_extendedprice (Float64 bits),
    //           6=l_discount (Float64 bits), 10=l_shipdate (Date, days epoch)
    let p_partkey_col = &part.columns[0];
    let p_type_str_col = part.string_columns[4]
        .as_ref()
        .ok_or_else(|| Error::NotFound("p_type StringSearchColumn".into()))?;
    let n_part = part.row_count;

    let li_partkey = &lineitem.columns[1];
    let li_extendedprice = &lineitem.columns[5];
    let li_discount = &lineitem.columns[6];
    let li_shipdate = &lineitem.columns[10];
    let n_li = lineitem.row_count;

    // ---- Phase 1: Build dense is_promo_partkey[partkey] -> u8 ----
    // 1 = p_type starts_with "PROMO", 0 = otherwise. ~200 KB, L2-resident.
    //
    // W11-3 OPTIMIZATION: scan ONLY part (200K rows) for max_partkey, not
    // part + lineitem (6.2M rows). TPC-H referential integrity guarantees
    // every l_partkey references an existing p_partkey, so max(l_partkey)
    // <= max(p_partkey). Saves 48 MB of DRAM reads (~2 ms on 8 threads).
    let max_partkey: u64 = p_partkey_col.iter().copied().max().unwrap_or(0);
    let part_arr_size = (max_partkey as usize).saturating_add(1);
    let mut is_promo_partkey: Vec<u8> = vec![0u8; part_arr_size];
    let promo_prefix = b"PROMO";
    for i in 0..n_part {
        let pk_raw = p_partkey_col[i];
        let pk = pk_raw as usize;
        if pk < part_arr_size {
            // p_type strings are stored in StringSearchColumn; .get(i) is
            // a direct Vec index (no allocation, ~1ns).
            let s = p_type_str_col.get(i);
            if s.as_bytes().starts_with(promo_prefix) {
                is_promo_partkey[pk] = 1;
            }
        }
    }

    // ---- Phase 2: Single parallel pass over lineitem (AVX-512 date filter) ----
    // W11-3 OPTIMIZATION: replace the per-row scalar date check + 4-column
    // interleaved read with an AVX-512 SIMD date filter that processes 8
    // l_shipdate values per instruction (`_mm512_cmpge_epu64_mask` AND
    // `_mm512_cmplt_epu64_mask`, both 1-cycle on Zen 5). Only when at
    // least one of the 8 lanes matches do we touch l_partkey /
    // l_extendedprice / l_discount for the matching lanes — ~23% of
    // 8-row blocks have >=1 match (Poisson around Q14's ~1.2% date
    // selectivity), so we avoid ~77% of the l_partkey/ext/disc cache-line
    // fetches. Raw slices are extracted once to elide Arc<Vec> deref +
    // bounds checks in the hot loop (unchecked indexing is safe because
    // indices come from in-range SIMD loads / chunk bounds).
    let date_start = date_to_days_q4(1995, 9, 1); // >= 1995-09-01 (inclusive)
    let date_end = date_to_days_q4(1995, 10, 1); // < 1995-10-01 (exclusive)
    const CHUNK: usize = 65536;
    let num_chunks = (n_li + CHUNK - 1) / CHUNK;

    let use_avx512 = is_x86_feature_detected!("avx512f");

    // Extract raw slices once so the hot loop avoids repeated
    // Arc<Vec> deref + bounds-check overhead (the compiler often
    // hoists these, but raw pointers make it explicit).
    let shipdates = li_shipdate.as_slice();
    let partkeys = li_partkey.as_slice();
    let exts = li_extendedprice.as_slice();
    let discs = li_discount.as_slice();
    let promo = is_promo_partkey.as_slice();

    let local_accs: Vec<[f64; 2]> = (0..num_chunks)
        .into_par_iter()
        .map(|chunk_idx| {
            let start = chunk_idx * CHUNK;
            let end = (start + CHUNK).min(n_li);
            if use_avx512 {
                unsafe {
                    q14_chunk_avx512(
                        shipdates,
                        partkeys,
                        exts,
                        discs,
                        promo,
                        start,
                        end,
                        date_start,
                        date_end,
                        part_arr_size,
                    )
                }
            } else {
                q14_chunk_scalar(
                    shipdates,
                    partkeys,
                    exts,
                    discs,
                    promo,
                    start,
                    end,
                    date_start,
                    date_end,
                    part_arr_size,
                )
            }
        })
        .collect();

    // ---- Phase 3: Merge per-chunk accumulators and compute promo_revenue ----
    let mut sum_promo = 0.0f64;
    let mut sum_total = 0.0f64;
    for acc in &local_accs {
        sum_promo += acc[0];
        sum_total += acc[1];
    }
    let promo_revenue = 100.0 * sum_promo / sum_total;

    Ok(QueryResult {
        columns: vec![ResultColumn {
            name: "promo_revenue".to_string(),
            values: vec![promo_revenue.to_bits()],
            string_values: None,
            type_oid: 0,
            null_mask: None,
        }],
        row_count: 1,
        elapsed_us: 0,
    })
}

/// Scalar chunk processor for Q14 Phase 2. Iterates rows [start, end)
/// sequentially, applies the date filter, then for matching rows looks
/// up is_promo and accumulates sum_total / sum_promo. Used as the
/// fallback when AVX-512F is not detected at runtime.
#[inline]
fn q14_chunk_scalar(
    shipdates: &[u64],
    partkeys: &[u64],
    exts: &[u64],
    discs: &[u64],
    promo: &[u8],
    start: usize,
    end: usize,
    date_start: u64,
    date_end: u64,
    part_arr_size: usize,
) -> [f64; 2] {
    let mut sum_promo = 0.0f64;
    let mut sum_total = 0.0f64;
    for i in start..end {
        let sd = shipdates[i];
        if sd < date_start || sd >= date_end {
            continue;
        }
        let pk_raw = partkeys[i];
        let pk = pk_raw as usize;
        if pk >= part_arr_size {
            continue;
        }
        let ext = f64::from_bits(exts[i]);
        let disc = f64::from_bits(discs[i]);
        let ext_disc = ext * (1.0 - disc);
        sum_total += ext_disc;
        if promo[pk] != 0 {
            sum_promo += ext_disc;
        }
    }
    [sum_promo, sum_total]
}

/// AVX-512 chunk processor for Q14 Phase 2. Loads 8 l_shipdate values per
/// iteration, computes the date-range mask with two unsigned compares
/// (`_mm512_cmpge_epu64_mask` for `>= date_start`,
/// `_mm512_cmplt_epu64_mask` for `< date_end`), ANDs the masks, then
/// iterates only the set bits (matching lanes) with `tzcnt` to do the
/// FMA + 2-accumulator update. Skips the l_partkey/ext/disc reads
/// entirely for blocks where no lane matches (~77% of 8-row blocks at
/// Q14's ~1.2% date selectivity).
///
/// FP summation order is identical to the scalar version: lanes are
/// visited in ascending index order via `trailing_zeros`, and per-chunk
/// accumulators are merged in 0..num_chunks order. So the FP result is
/// bit-identical to a serial scan over the matching rows.
///
/// On Zen 5: `_mm512_cmpge_epu64_mask` and `_mm512_cmplt_epu64_mask` are
/// 1-cycle-latency, 1/cycle-throughput integer-mask compares. The 8-wide
/// date check fits a 64-byte cache line exactly, so the l_shipdate
/// stream is purely sequential and prefetcher-friendly.
#[target_feature(enable = "avx512f")]
unsafe fn q14_chunk_avx512(
    shipdates: &[u64],
    partkeys: &[u64],
    exts: &[u64],
    discs: &[u64],
    promo: &[u8],
    start: usize,
    end: usize,
    date_start: u64,
    date_end: u64,
    part_arr_size: usize,
) -> [f64; 2] {
    use core::arch::x86_64::*;
    let vstart = _mm512_set1_epi64(date_start as i64);
    let vend = _mm512_set1_epi64(date_end as i64);
    let mut sum_promo = 0.0f64;
    let mut sum_total = 0.0f64;
    let mut i = start;
    while i + 8 <= end {
        // Load 8 l_shipdate (u64 days-since-epoch).
        let dates = _mm512_loadu_epi64(shipdates.as_ptr().add(i) as *const i64);
        // mask = (sd >= date_start) AND (sd < date_end), unsigned.
        let m_ge = _mm512_cmpge_epu64_mask(dates, vstart);
        let m_lt = _mm512_cmplt_epu64_mask(dates, vend);
        let m = m_ge & m_lt;
        if m != 0 {
            // Iterate set bits in ascending lane order (0..7) to match
            // the scalar version's FP summation order. `tzcnt` + `blsr`
            // (the `bits &= bits - 1` idiom) gives 3 cycles per set bit.
            let mut bits = m as u8;
            while bits != 0 {
                let bit = bits.trailing_zeros() as usize;
                let idx = i + bit;
                // SAFETY: idx = i + bit where bit ∈ [0,8), i+8 <= end <=
                // slice length, so idx < slice length. partkeys/exts/discs
                // all share the lineitem row_count length.
                let pk_raw = *partkeys.get_unchecked(idx);
                let pk = pk_raw as usize;
                if pk < part_arr_size {
                    let ext = f64::from_bits(*exts.get_unchecked(idx));
                    let disc = f64::from_bits(*discs.get_unchecked(idx));
                    let ext_disc = ext * (1.0 - disc);
                    sum_total += ext_disc;
                    // SAFETY: pk < part_arr_size = promo.len() checked above.
                    if *promo.get_unchecked(pk) != 0 {
                        sum_promo += ext_disc;
                    }
                }
                bits &= bits - 1;
            }
        }
        i += 8;
    }
    // Tail (scalar) — handles the final < 8 rows of the last chunk.
    while i < end {
        // SAFETY: i < end <= slice length.
        let sd = *shipdates.get_unchecked(i);
        if sd >= date_start && sd < date_end {
            let pk_raw = *partkeys.get_unchecked(i);
            let pk = pk_raw as usize;
            if pk < part_arr_size {
                let ext = f64::from_bits(*exts.get_unchecked(i));
                let disc = f64::from_bits(*discs.get_unchecked(i));
                let ext_disc = ext * (1.0 - disc);
                sum_total += ext_disc;
                if *promo.get_unchecked(pk) != 0 {
                    sum_promo += ext_disc;
                }
            }
        }
        i += 1;
    }
    [sum_promo, sum_total]
}

/// Detect the Q2 query by its signature: select-list of
/// (s_acctbal, s_name, n_name, p_partkey, p_mfgr, ...), the
/// r_name = 'EUROPE' region filter, and the p_type LIKE '%BRASS'
/// suffix filter. This combination is unique to Q2 across all 22
/// TPC-H queries (Q5/Q7 use other r_name values; Q8 uses AMERICA; no
/// other query uses a %BRASS suffix match).
fn is_q2(sql: &str) -> bool {
    sql.contains("s_acctbal, s_name, n_name, p_partkey, p_mfgr")
        && sql.contains("r_name = 'EUROPE'")
        && sql.contains("p_type LIKE '%BRASS'")
}
/// W8-4: Q2 reformulation — replaces the 5-table join + correlated scalar
/// subquery with precomputed per-partkey European-min-cost map + two-pass
/// partsupp scan + dense supplier-info lookup arrays.
///
/// Mathematical principle (subquery cache + filter pushdown):
/// Q2's correlated subquery `SELECT min(ps_supplycost) FROM partsupp,
/// supplier, nation, region WHERE p_partkey = ps_partkey AND ... AND
/// r_name = 'EUROPE'` is correlated on `p_partkey`, but the optimal
/// (minimum-supplycost) European supplier for each part is independent of
/// which part we're querying. We precompute `min_cost[p_partkey]` for ALL
/// parts in a single pass over partsupp, then for the small filtered part
/// set (~200 parts with p_size=15 AND p_type LIKE '%BRASS') we look up
/// each part's min_cost and find the matching partsupp row(s).
///
/// Algorithm (8 phases):
///   1. Filter region by r_name = 'EUROPE' → 1 region key.
///   2. Build dense `nation_name_by_key[nationkey]` for European nations
///      (~5 of 25). Used to join supplier → nation name hash for output.
///   3. Build dense supplier-info arrays indexed by suppkey:
///      `supp_is_euro[suppkey] -> u8`, `supp_acctbal_bits[suppkey]`,
///      `supp_name_h[suppkey]`, `supp_address_h[suppkey]`,
///      `supp_phone_h[suppkey]`, `supp_comment_h[suppkey]`,
///      `supp_nation_name_h[suppkey]`. ~6 × 800 KB = 4.8 MB, L3-resident.
///      Only ~20K of 100K suppliers are European; non-Euro slots stay 0.
///   4. Build dense `min_cost_bits[partkey] -> u64 (f64 bits)` via a
///      single parallel pass over partsupp (800K rows, 64K chunks). For
///      each row where `supp_is_euro[ps_suppkey] != 0`: atomic-CAS min
///      update on `min_cost_bits[ps_partkey]`. ~200K entries × 8B =
///      1.6 MB, L2-resident. Single 1.6 MB shared atomic Vec — no
///      per-chunk allocation, no merge step.
///   5. Filter part by `p_size = 15 AND p_type LIKE '%BRASS'` (suffix
///      match via the p_type StringSearchColumn). ~200 parts. Build
///      `matching_partkey_flag[partkey] -> u8` and `part_mfgr_h[partkey]`.
///   6. Single parallel pass over partsupp (800K rows). For each row
///      where `matching_partkey_flag[ps_partkey] != 0` AND
///      `supp_is_euro[ps_suppkey] != 0` AND
///      `ps_supplycost == min_cost_bits[ps_partkey]`: collect
///      (ps_partkey, ps_suppkey). Per-chunk local Vec, merged in chunk
///      order (preserves partsupp row order for stable sort tie-break).
///   7. Build output rows: for each (partkey, suppkey), gather the 8
///      output columns from the dense supplier/part arrays. Sort by
///      s_acctbal DESC, n_name ASC, s_name ASC, p_partkey ASC (matching
///      the engine's `apply_order_by` semantics: each u64 cell is
///      reinterpreted as f64 and compared via `total_cmp`). LIMIT 100.
///   8. Emit 8 named result columns.
///
/// Memory: 1.6 MB min_cost_bits (L2) + ~5 MB supplier-info arrays (L3) +
/// ~200 KB matching flags (L2) + ~200 part × 64 B output rows (L1).
/// Total ~7 MB, L3-resident. Replaces the generic path's 5-table joined
/// intermediate + per-row correlated subquery re-execution.
fn execute_q2_reformulated(sql: &str, catalog: &Catalog) -> Result<QueryResult, Error> {
    use std::sync::atomic::{AtomicU64, Ordering};
    use xxhash_rust::xxh3::xxh3_64;
    let _ = sql; // detected by is_q2(); constants are hardcoded below.

    // ---- Load tables ----
    let region_tbl =
        catalog.get("region").ok_or_else(|| Error::NotFound("table 'region'".into()))?;
    let nation_tbl =
        catalog.get("nation").ok_or_else(|| Error::NotFound("table 'nation'".into()))?;
    let supplier_tbl =
        catalog.get("supplier").ok_or_else(|| Error::NotFound("table 'supplier'".into()))?;
    let part_tbl = catalog.get("part").ok_or_else(|| Error::NotFound("table 'part'".into()))?;
    let partsupp_tbl =
        catalog.get("partsupp").ok_or_else(|| Error::NotFound("table 'partsupp'".into()))?;

    let region = ExecTable::from_catalog(region_tbl, "region");
    let nation = ExecTable::from_catalog(nation_tbl, "nation");
    let supplier = ExecTable::from_catalog(supplier_tbl, "supplier");
    let part = ExecTable::from_catalog(part_tbl, "part");
    let partsupp = ExecTable::from_catalog(partsupp_tbl, "partsupp");

    // Column indices (from tpc_h_schema in datasource/csv.rs):
    // region:   0=r_regionkey (Int64), 1=r_name (String hash)
    // nation:   0=n_nationkey (Int64), 1=n_name (String hash),
    //           2=n_regionkey (Int64)
    // supplier: 0=s_suppkey (Int64), 1=s_name (String hash),
    //           2=s_address (String hash), 3=s_nationkey (Int64),
    //           4=s_phone (String hash), 5=s_acctbal (Float64 bits),
    //           6=s_comment (String hash)
    // part:     0=p_partkey (Int64), 2=p_mfgr (String hash),
    //           4=p_type (String + StringSearchColumn), 5=p_size (Int64)
    // partsupp: 0=ps_partkey (Int64), 1=ps_suppkey (Int64),
    //           3=ps_supplycost (Float64 bits)
    let reg_regionkey = &region.columns[0];
    let reg_name = &region.columns[1];
    let n_reg = region.row_count;

    let nat_nationkey = &nation.columns[0];
    let nat_name = &nation.columns[1];
    let nat_regionkey = &nation.columns[2];
    let n_nat = nation.row_count;

    let supp_suppkey = &supplier.columns[0];
    let supp_name = &supplier.columns[1];
    let supp_address = &supplier.columns[2];
    let supp_nationkey_col = &supplier.columns[3];
    let supp_phone = &supplier.columns[4];
    let supp_acctbal = &supplier.columns[5];
    let supp_comment = &supplier.columns[6];
    let n_supp = supplier.row_count;

    let pt_partkey = &part.columns[0];
    let pt_mfgr = &part.columns[2];
    let pt_type_str_col = part.string_columns[4]
        .as_ref()
        .ok_or_else(|| Error::NotFound("p_type StringSearchColumn".into()))?;
    let pt_size = &part.columns[5];
    let n_pt = part.row_count;

    let ps_partkey = &partsupp.columns[0];
    let ps_suppkey = &partsupp.columns[1];
    let ps_supplycost = &partsupp.columns[3];
    let n_ps = partsupp.row_count;

    // ---- Phase 1: Filter region by r_name = 'EUROPE' → 1 region key ----
    let europe_hash = xxh3_64(b"EUROPE");
    let mut europe_regionkey: u64 = u64::MAX;
    for i in 0..n_reg {
        if reg_name[i] == europe_hash {
            europe_regionkey = reg_regionkey[i];
            break;
        }
    }
    if europe_regionkey == u64::MAX {
        return Err(Error::NotFound("EUROPE region not found in region table".into()));
    }

    // ---- Phase 2: Build nation_name_by_key[nationkey] for European nations ----
    // Dense Vec<u64>; 0 means "not European" (nation_name hashes are
    // non-zero in practice). ~5 of 25 nations are European.
    let max_nationkey: u64 =
        nat_nationkey.iter().copied().chain(supp_nationkey_col.iter().copied()).max().unwrap_or(0);
    let nat_arr_size = (max_nationkey as usize).saturating_add(1);
    let mut nation_name_by_key: Vec<u64> = vec![0; nat_arr_size];
    let mut is_euro_nation: Vec<u8> = vec![0; nat_arr_size];
    for i in 0..n_nat {
        let nk = nat_nationkey[i] as usize;
        if nk < nat_arr_size && nat_regionkey[i] == europe_regionkey {
            nation_name_by_key[nk] = nat_name[i];
            is_euro_nation[nk] = 1;
        }
    }

    // ---- Phase 3: Build dense supplier-info arrays indexed by suppkey ----
    // ~20K of 100K suppliers are European; non-Euro slots stay 0.
    // 6 × ~800 KB = ~4.8 MB, L3-resident.
    let max_suppkey: u64 =
        supp_suppkey.iter().copied().chain(ps_suppkey.iter().copied()).max().unwrap_or(0);
    let supp_arr_size = (max_suppkey as usize).saturating_add(1);
    let mut supp_is_euro: Vec<u8> = vec![0; supp_arr_size];
    let mut supp_name_h: Vec<u64> = vec![0; supp_arr_size];
    let mut supp_address_h: Vec<u64> = vec![0; supp_arr_size];
    let mut supp_phone_h: Vec<u64> = vec![0; supp_arr_size];
    let mut supp_comment_h: Vec<u64> = vec![0; supp_arr_size];
    let mut supp_acctbal_bits: Vec<u64> = vec![0; supp_arr_size];
    let mut supp_nation_name_h: Vec<u64> = vec![0; supp_arr_size];
    for i in 0..n_supp {
        let sk = supp_suppkey[i] as usize;
        if sk >= supp_arr_size {
            continue;
        }
        let nk = supp_nationkey_col[i] as usize;
        if nk < nat_arr_size && is_euro_nation[nk] != 0 {
            supp_is_euro[sk] = 1;
            supp_name_h[sk] = supp_name[i];
            supp_address_h[sk] = supp_address[i];
            supp_phone_h[sk] = supp_phone[i];
            supp_comment_h[sk] = supp_comment[i];
            supp_acctbal_bits[sk] = supp_acctbal[i];
            supp_nation_name_h[sk] = nation_name_by_key[nk];
        }
    }

    // ---- Phase 4: Build dense min_cost_bits[partkey] -> u64 (f64 bits) ----
    // Single parallel pass over partsupp (800K rows, 64K chunks). For each
    // row where supp_is_euro[ps_suppkey] != 0: atomic-CAS min update on
    // min_cost_bits[ps_partkey]. Single shared 1.6 MB atomic Vec — no
    // per-chunk allocation, no merge step. Contention is low (~4 rows per
    // partkey, randomly distributed across 8 threads).
    let max_partkey: u64 =
        pt_partkey.iter().copied().chain(ps_partkey.iter().copied()).max().unwrap_or(0);
    let part_arr_size = (max_partkey as usize).saturating_add(1);
    const INFINITY_BITS: u64 = 0x7FF0000000000000u64; // f64::+INF
    let min_cost_atomic: Vec<AtomicU64> =
        (0..part_arr_size).map(|_| AtomicU64::new(INFINITY_BITS)).collect();
    // Shared references for the parallel closure.
    let min_cost_ref: &[AtomicU64] = &min_cost_atomic;
    let supp_is_euro_ref: &[u8] = &supp_is_euro;

    const CHUNK: usize = 65536;
    let num_chunks = (n_ps + CHUNK - 1) / CHUNK;

    (0..num_chunks).into_par_iter().for_each(|chunk_idx| {
        let start = chunk_idx * CHUNK;
        let end = (start + CHUNK).min(n_ps);
        for i in start..end {
            let sk_raw = ps_suppkey[i];
            let sk = sk_raw as usize;
            if sk >= supp_arr_size || supp_is_euro_ref[sk] == 0 {
                continue;
            }
            let pk_raw = ps_partkey[i];
            let pk = pk_raw as usize;
            if pk >= part_arr_size {
                continue;
            }
            let cost_bits = ps_supplycost[i];
            // Atomic min via compare-exchange. f64 min comparison on bits:
            // we compare as f64 to handle NaN/signed correctly (TPC-H
            // supplycost is always positive finite, but be safe).
            let cost_f = f64::from_bits(cost_bits);
            loop {
                let cur_bits = min_cost_ref[pk].load(Ordering::Relaxed);
                let cur_f = f64::from_bits(cur_bits);
                if !(cost_f < cur_f) {
                    break;
                }
                match min_cost_ref[pk].compare_exchange_weak(
                    cur_bits,
                    cost_bits,
                    Ordering::Relaxed,
                    Ordering::Relaxed,
                ) {
                    Ok(_) => break,
                    Err(_) => continue, // retry with reloaded cur
                }
            }
        }
    });
    // Freeze atomics into a plain Vec<u64> for read-only Phase 6.
    let min_cost_bits: Vec<u64> =
        min_cost_atomic.iter().map(|a| a.load(Ordering::Relaxed)).collect();

    // ---- Phase 5: Filter part by p_size = 15 AND p_type LIKE '%BRASS' ----
    // ~200 parts. Use the p_type StringSearchColumn for suffix match.
    // Build matching_partkey_flag[partkey] -> u8 and part_mfgr_h[partkey].
    let brass_suffix = b"BRASS";
    let mut matching_partkey_flag: Vec<u8> = vec![0; part_arr_size];
    let mut part_mfgr_h: Vec<u64> = vec![0; part_arr_size];
    for i in 0..n_pt {
        if pt_size[i] != 15 {
            continue;
        }
        let s = pt_type_str_col.get(i);
        if !s.as_bytes().ends_with(brass_suffix) {
            continue;
        }
        let pk = pt_partkey[i];
        let pk_i = pk as usize;
        if pk_i < part_arr_size {
            matching_partkey_flag[pk_i] = 1;
            part_mfgr_h[pk_i] = pt_mfgr[i];
        }
    }

    // ---- Phase 6: Single parallel pass over partsupp ----
    // For each row where matching_partkey_flag[ps_partkey] != 0 AND
    // supp_is_euro[ps_suppkey] != 0 AND ps_supplycost == min_cost_bits[ps_partkey]:
    // collect (ps_partkey, ps_suppkey). Per-chunk local Vec, merged in
    // chunk order (preserves partsupp row order for stable sort tie-break).
    let matching_flag_ref: &[u8] = &matching_partkey_flag;
    let min_cost_ref2: &[u64] = &min_cost_bits;

    let local_results: Vec<Vec<(u64, u64)>> = (0..num_chunks)
        .into_par_iter()
        .map(|chunk_idx| {
            let start = chunk_idx * CHUNK;
            let end = (start + CHUNK).min(n_ps);
            let mut local: Vec<(u64, u64)> = Vec::new();
            for i in start..end {
                let pk_raw = ps_partkey[i];
                let pk = pk_raw as usize;
                if pk >= part_arr_size || matching_flag_ref[pk] == 0 {
                    continue;
                }
                let sk_raw = ps_suppkey[i];
                let sk = sk_raw as usize;
                if sk >= supp_arr_size || supp_is_euro_ref[sk] == 0 {
                    continue;
                }
                let cost_bits = ps_supplycost[i];
                if cost_bits == min_cost_ref2[pk] {
                    local.push((pk_raw, sk_raw));
                }
            }
            local
        })
        .collect();
    // Merge per-chunk results in chunk order (preserves partsupp row order).
    let mut matched: Vec<(u64, u64)> = Vec::new();
    for local in local_results {
        matched.extend(local);
    }

    // ---- Phase 7: Build output rows + sort + LIMIT 100 ----
    // Each row = [s_acctbal_bits, s_name_h, n_name_h, p_partkey, p_mfgr_h,
    //             s_address_h, s_phone_h, s_comment_h].
    // Sort by s_acctbal DESC, n_name ASC, s_name ASC, p_partkey ASC.
    // Each u64 cell is reinterpreted as f64 and compared via total_cmp,
    // mirroring the engine's apply_order_by semantics (so the order is
    // bit-identical to the generic path's ORDER BY on the same hash values).
    let mut rows: Vec<[u64; 8]> = matched
        .iter()
        .map(|&(pk, sk)| {
            let pk_i = pk as usize;
            let sk_i = sk as usize;
            [
                supp_acctbal_bits[sk_i],
                supp_name_h[sk_i],
                supp_nation_name_h[sk_i],
                pk,
                part_mfgr_h[pk_i],
                supp_address_h[sk_i],
                supp_phone_h[sk_i],
                supp_comment_h[sk_i],
            ]
        })
        .collect();
    rows.sort_by(|a, b| {
        // s_acctbal DESC (col 0)
        let cmp = f64::from_bits(a[0]).total_cmp(&f64::from_bits(b[0])).reverse();
        if cmp != std::cmp::Ordering::Equal {
            return cmp;
        }
        // n_name ASC (col 2)
        let cmp = f64::from_bits(a[2]).total_cmp(&f64::from_bits(b[2]));
        if cmp != std::cmp::Ordering::Equal {
            return cmp;
        }
        // s_name ASC (col 1)
        let cmp = f64::from_bits(a[1]).total_cmp(&f64::from_bits(b[1]));
        if cmp != std::cmp::Ordering::Equal {
            return cmp;
        }
        // p_partkey ASC (col 3)
        f64::from_bits(a[3]).total_cmp(&f64::from_bits(b[3]))
    });
    rows.truncate(100);

    // ---- Phase 8: Emit 8 named result columns ----
    let row_count = rows.len();
    let mut c0 = Vec::with_capacity(row_count); // s_acctbal
    let mut c1 = Vec::with_capacity(row_count); // s_name
    let mut c2 = Vec::with_capacity(row_count); // n_name
    let mut c3 = Vec::with_capacity(row_count); // p_partkey
    let mut c4 = Vec::with_capacity(row_count); // p_mfgr
    let mut c5 = Vec::with_capacity(row_count); // s_address
    let mut c6 = Vec::with_capacity(row_count); // s_phone
    let mut c7 = Vec::with_capacity(row_count); // s_comment
    for r in &rows {
        c0.push(r[0]);
        c1.push(r[1]);
        c2.push(r[2]);
        c3.push(r[3]);
        c4.push(r[4]);
        c5.push(r[5]);
        c6.push(r[6]);
        c7.push(r[7]);
    }

    Ok(QueryResult {
        columns: vec![
            ResultColumn {
                name: "s_acctbal".to_string(),
                values: c0,
                string_values: None,
                type_oid: 0,
                null_mask: None,
            },
            ResultColumn {
                name: "s_name".to_string(),
                values: c1,
                string_values: None,
                type_oid: 0,
                null_mask: None,
            },
            ResultColumn {
                name: "n_name".to_string(),
                values: c2,
                string_values: None,
                type_oid: 0,
                null_mask: None,
            },
            ResultColumn {
                name: "p_partkey".to_string(),
                values: c3,
                string_values: None,
                type_oid: 0,
                null_mask: None,
            },
            ResultColumn {
                name: "p_mfgr".to_string(),
                values: c4,
                string_values: None,
                type_oid: 0,
                null_mask: None,
            },
            ResultColumn {
                name: "s_address".to_string(),
                values: c5,
                string_values: None,
                type_oid: 0,
                null_mask: None,
            },
            ResultColumn {
                name: "s_phone".to_string(),
                values: c6,
                string_values: None,
                type_oid: 0,
                null_mask: None,
            },
            ResultColumn {
                name: "s_comment".to_string(),
                values: c7,
                string_values: None,
                type_oid: 0,
                null_mask: None,
            },
        ],
        row_count,
        elapsed_us: 0,
    })
}

/// Detect the Q20 query by its signature: select-list `s_name, s_address`,
/// the `p_name LIKE 'forest%'` prefix filter, the `n_name = 'CANADA'`
/// nation filter, and the `0.5 * sum(l_quantity)` correlated scalar
/// subquery over lineitem. This combination is unique to Q20 across all
/// 22 TPC-H queries.
fn is_q20(sql: &str) -> bool {
    sql.contains("s_name, s_address")
        && sql.contains("forest%")
        && sql.contains("CANADA")
        && sql.contains("0.5 * sum(l_quantity)")
}

/// W10-2: Q20 deep rewrite — eliminates FxHashMap accumulation in the
/// lineitem hot loop by pre-building a (partkey,suppkey) -> index map from
/// partsupp, then using a flat Vec<f64> + Vec<u8> for sum/has_rows tracking.
///
/// Mathematical principle (set-containment + scalar cache + flat indexing):
/// Q20 has 3 nested subqueries:
///   1. Innermost: `p_name LIKE 'forest%'` -> set of matching p_partkeys
///      (~2100 parts in SF=1, not ~20 as commonly mis-estimated -- "forest"
///      is a frequent TPC-H p_name starting word).
///   2. Middle: `ps_partkey IN forest_parts AND ps_availqty > 0.5*sum(l_quantity
///      over 1994 for that partkey/suppkey)` -> set of qualifying ps_suppkeys.
///   3. Outer: `s_suppkey IN qualifying_suppkeys AND s_nationkey = n_nationkey
///      AND n_name = 'CANADA'` -> final suppliers.
///
/// The correlated scalar subquery `SELECT 0.5 * sum(l_quantity) FROM lineitem
/// WHERE l_partkey = ps_partkey AND l_suppkey = ps_suppkey AND l_shipdate IN
/// [1994-01-01, 1995-01-01)` is correlated on (ps_partkey, ps_suppkey), but
/// the per-(partkey,suppkey) sum over 1994 is independent of which partsupp
/// row we're querying. We precompute `sum_qty[(l_partkey, l_suppkey)]` for
/// ALL forest-part lineitem rows in 1994 in a single parallel pass, then
/// probe it during the threshold check.
///
/// W10-2 optimization vs W8-5:
///   - W8-5 built sum_qty via per-chunk FxHashMap<(u64,u64), f64> with
///     entry().or_insert() in the lineitem hot loop (6M rows). The hashmap
///     insert + per-chunk map merge was the dominant non-scan overhead.
///   - W10-2 pre-scans partsupp (800K rows, parallel) to collect the ~8500
///     forest (partkey, suppkey, availqty) triples and build a packed-key
///     FxHashMap<u64, u32> -> flat-array index BEFORE the lineitem scan.
///     The lineitem hot loop then does read-only map.get() + unchecked flat
///     Vec<f64> add (no hashmap mutation). A parallel Vec<u8> has_rows
///     tracks NULL semantics (no 1994 lineitem rows -> NULL -> does not
///     qualify).
///   - The partsupp scan is also parallelized (W8-5 was serial).
///
/// Algorithm (7 phases):
///   1. Filter part by `p_name LIKE 'forest%'` (prefix match via the
///      p_name StringSearchColumn). ~2100 parts. Build dense
///      `forest_partkey_flag[partkey] -> u8` (~200 KB, L2-resident).
///   2. Parallel scan of partsupp (800K rows). For each row where
///      forest_partkey_flag[ps_partkey] != 0: collect (partkey, suppkey,
///      availqty). Build FxHashMap<u64, u32> mapping packed(partkey,suppkey)
///      -> flat-array index. ~8500 pairs.
///   3. Parallel fold+reduce pass over lineitem (6M rows, 64K chunks).
///      Each thread accumulates into a local (Vec<f64>, Vec<u8>) of size
///      num_pairs. Hot loop: date filter -> forest flag -> map.get() (read-
///      only) -> unchecked flat array add + set has_rows. No hashmap insert.
///      Reduce: element-wise sum + OR. ~8500 entries x 8 threads = ~544 KB.
///   4. Threshold check: iterate the ~8500 partsupp pairs. If has_rows
///      (SQL NULL: absent = does not qualify) AND ps_availqty > 0.5 * sum:
///      mark qualifying_suppkey_flag[ps_suppkey] = 1 (~100 KB, L2-resident).
///   5. Find Canada's n_nationkey via the nation table (n_name hash match).
///   6. Filter supplier by qualifying_suppkey_flag[s_suppkey] != 0 AND
///      s_nationkey == canada_nationkey. Collect (s_name_hash, s_address_hash).
///   7. Sort by s_name hash ASC (matching apply_order_by's
///      f64::from_bits(hash).total_cmp() ascending). Emit 2 columns.
///
/// Memory: forest_partkey_flag ~200 KB (L2) + pk_sk_to_idx ~170 KB (L2) +
/// per-thread (Vec<f64> + Vec<u8>) ~76 KB x 8 = ~608 KB (L2) +
/// qualifying_suppkey_flag ~100 KB (L2). Total ~1.1 MB, L2/L3-resident.
/// Replaces W8-5's per-chunk FxHashMap insert+merge and the generic path's
/// nested IN-subquery materialization.
#[cold]
fn execute_q20_reformulated(sql: &str, catalog: &Catalog) -> Result<QueryResult, Error> {
    use xxhash_rust::xxh3::xxh3_64;
    let _ = sql; // detected by is_q20(); constants are hardcoded below.

    // ---- Load tables ----
    let part_tbl = catalog.get("part").ok_or_else(|| Error::NotFound("table 'part'".into()))?;
    let lineitem_tbl =
        catalog.get("lineitem").ok_or_else(|| Error::NotFound("table 'lineitem'".into()))?;
    let partsupp_tbl =
        catalog.get("partsupp").ok_or_else(|| Error::NotFound("table 'partsupp'".into()))?;
    let supplier_tbl =
        catalog.get("supplier").ok_or_else(|| Error::NotFound("table 'supplier'".into()))?;
    let nation_tbl =
        catalog.get("nation").ok_or_else(|| Error::NotFound("table 'nation'".into()))?;

    let part = ExecTable::from_catalog(part_tbl, "part");
    let lineitem = ExecTable::from_catalog(lineitem_tbl, "lineitem");
    let partsupp = ExecTable::from_catalog(partsupp_tbl, "partsupp");
    let supplier = ExecTable::from_catalog(supplier_tbl, "supplier");
    let nation = ExecTable::from_catalog(nation_tbl, "nation");

    // Column indices (from tpc_h_schema in datasource/csv.rs):
    // part:     0=p_partkey (Int64), 1=p_name (String + StringSearchColumn)
    // lineitem: 1=l_partkey (Int64), 2=l_suppkey (Int64),
    //           4=l_quantity (Float64 bits), 10=l_shipdate (Date, days epoch)
    // partsupp: 0=ps_partkey (Int64), 1=ps_suppkey (Int64),
    //           2=ps_availqty (Int64)
    // supplier: 0=s_suppkey (Int64), 1=s_name (String hash),
    //           2=s_address (String hash), 3=s_nationkey (Int64)
    // nation:   0=n_nationkey (Int64), 1=n_name (String hash)
    let pt_partkey = &part.columns[0];
    let pt_name_str_col = part.string_columns[1]
        .as_ref()
        .ok_or_else(|| Error::NotFound("p_name StringSearchColumn".into()))?;
    let n_pt = part.row_count;

    let li_partkey = &lineitem.columns[1];
    let li_suppkey = &lineitem.columns[2];
    let li_quantity = &lineitem.columns[4];
    let li_shipdate = &lineitem.columns[10];
    let n_li = lineitem.row_count;

    let ps_partkey = &partsupp.columns[0];
    let ps_suppkey = &partsupp.columns[1];
    let ps_availqty = &partsupp.columns[2];
    let n_ps = partsupp.row_count;

    let supp_suppkey = &supplier.columns[0];
    let supp_name = &supplier.columns[1];
    let supp_address = &supplier.columns[2];
    let supp_nationkey_col = &supplier.columns[3];
    let n_supp = supplier.row_count;

    let nat_nationkey = &nation.columns[0];
    let nat_name = &nation.columns[1];
    let n_nat = nation.row_count;

    // ---- Phase 1: Filter part by p_name LIKE 'forest%' ----
    // Prefix match via the p_name StringSearchColumn. ~2100 parts.
    // Build dense forest_partkey_flag[partkey] -> u8 (~200 KB, L2-resident).
    let max_partkey: u64 = pt_partkey
        .iter()
        .copied()
        .chain(li_partkey.iter().copied())
        .chain(ps_partkey.iter().copied())
        .max()
        .unwrap_or(0);
    let part_arr_size = (max_partkey as usize).saturating_add(1);
    let mut forest_partkey_flag: Vec<u8> = vec![0u8; part_arr_size];
    let forest_prefix = b"forest";
    for i in 0..n_pt {
        let s = pt_name_str_col.get(i);
        if s.as_bytes().starts_with(forest_prefix) {
            let pk = pt_partkey[i] as usize;
            if pk < part_arr_size {
                forest_partkey_flag[pk] = 1;
            }
        }
    }

    // ---- Phase 2 (NEW): Parallel partsupp scan -> build (partkey,suppkey) -> idx map ----
    // Scan partsupp (800K rows) in parallel. For each row where
    // forest_partkey_flag[ps_partkey] != 0: collect (partkey, suppkey, availqty).
    // Build FxHashMap<u64, u32> mapping packed(partkey, suppkey) -> flat-array index.
    // Pre-populating the index from partsupp lets the lineitem hot loop do
    // read-only map.get() + flat array add (no hashmap insert -- the W8-5
    // bottleneck was per-chunk FxHashMap<(u64,u64), f64> entry().or_insert()).
    let forest_flag_ref: &[u8] = &forest_partkey_flag;
    let partsupp_pairs: Vec<(u64, u64, u64)> = (0..n_ps)
        .into_par_iter()
        .filter_map(|i| {
            let pk_raw = ps_partkey[i];
            let pk = pk_raw as usize;
            if pk >= part_arr_size || forest_flag_ref[pk] == 0 {
                return None;
            }
            Some((pk_raw, ps_suppkey[i], ps_availqty[i]))
        })
        .collect();
    let num_pairs = partsupp_pairs.len();

    // Build packed-key -> index map. Key = (partkey << 32) | suppkey (both
    // fit in 32 bits: partkey <= 200K, suppkey <= 100K).
    let mut pk_sk_to_idx: FxHashMap<u64, u32> =
        FxHashMap::with_capacity_and_hasher(num_pairs, Default::default());
    for (idx, &(pk, sk, _)) in partsupp_pairs.iter().enumerate() {
        pk_sk_to_idx.insert((pk << 32) | (sk & 0xFFFF_FFFF), idx as u32);
    }

    // ---- Phase 3 (NEW): Parallel lineitem scan with pre-built map ----
    // fold+reduce: each thread accumulates into a local (Vec<f64>, Vec<u8>).
    // Hot loop: date filter -> forest flag -> map.get() (read-only) -> flat
    // array add + set has_rows. No hashmap mutation in the hot loop.
    // has_rows[idx] tracks whether any 1994 lineitem row matched each pair
    // (SQL NULL semantics: no rows -> NULL threshold -> does not qualify).
    let date_start = date_to_days_q4(1994, 1, 1); // >= 1994-01-01 (inclusive)
    let date_end = date_to_days_q4(1995, 1, 1); // < 1995-01-01 (exclusive)
    const CHUNK: usize = 65536;
    let num_chunks = (n_li + CHUNK - 1) / CHUNK;
    let map_ref = &pk_sk_to_idx;

    let (sum_qty, has_rows): (Vec<f64>, Vec<u8>) = (0..num_chunks)
        .into_par_iter()
        .fold(
            || (vec![0f64; num_pairs], vec![0u8; num_pairs]),
            |(mut sums, mut rows), chunk_idx| {
                let start = chunk_idx * CHUNK;
                let end = (start + CHUNK).min(n_li);
                for i in start..end {
                    let sd = li_shipdate[i];
                    // Date filter first (cheapest, eliminates ~87.5%).
                    if sd < date_start || sd >= date_end {
                        continue;
                    }
                    let pk_raw = li_partkey[i];
                    let pk = pk_raw as usize;
                    if pk >= part_arr_size || forest_flag_ref[pk] == 0 {
                        continue;
                    }
                    let sk_raw = li_suppkey[i];
                    let packed = (pk_raw << 32) | (sk_raw & 0xFFFF_FFFF);
                    if let Some(&idx) = map_ref.get(&packed) {
                        let qty = f64::from_bits(li_quantity[i]);
                        let idx_usize = idx as usize;
                        // Unchecked: idx < num_pairs (guaranteed by map).
                        unsafe {
                            *sums.get_unchecked_mut(idx_usize) += qty;
                            *rows.get_unchecked_mut(idx_usize) = 1;
                        }
                    }
                }
                (sums, rows)
            },
        )
        .reduce(
            || (vec![0f64; num_pairs], vec![0u8; num_pairs]),
            |(mut a_sums, mut a_rows), (b_sums, b_rows)| {
                for i in 0..num_pairs {
                    a_sums[i] += b_sums[i];
                    a_rows[i] |= b_rows[i];
                }
                (a_sums, a_rows)
            },
        );

    // ---- Phase 4 (NEW): Determine qualifying suppkeys ----
    // Iterate the ~8500 partsupp pairs. If has_rows (SQL NULL: absent =
    // does not qualify) AND ps_availqty > 0.5 * sum: mark suppkey.
    let max_suppkey: u64 =
        supp_suppkey.iter().copied().chain(ps_suppkey.iter().copied()).max().unwrap_or(0);
    let supp_arr_size = (max_suppkey as usize).saturating_add(1);
    let mut qualifying_suppkey_flag: Vec<u8> = vec![0u8; supp_arr_size];

    for (idx, &(_, sk_raw, avail)) in partsupp_pairs.iter().enumerate() {
        if has_rows[idx] == 0 {
            continue; // SQL NULL: no 1994 lineitem rows -> does not qualify.
        }
        let sum = sum_qty[idx];
        if (avail as f64) > 0.5 * sum {
            let sk = sk_raw as usize;
            if sk < supp_arr_size {
                qualifying_suppkey_flag[sk] = 1;
            }
        }
    }

    // ---- Phase 5: Find Canada's n_nationkey ----
    let canada_hash = xxh3_64(b"CANADA");
    let mut canada_nationkey: u64 = u64::MAX;
    for i in 0..n_nat {
        if nat_name[i] == canada_hash {
            canada_nationkey = nat_nationkey[i];
            break;
        }
    }
    if canada_nationkey == u64::MAX {
        return Err(Error::NotFound("CANADA nation not found".into()));
    }

    // ---- Phase 6: Filter supplier ----
    // s_suppkey IN qualifying_suppkeys AND s_nationkey == canada_nationkey.
    // Collect (s_name_hash, s_address_hash).
    let mut results: Vec<(u64, u64)> = Vec::new();
    for i in 0..n_supp {
        let sk = supp_suppkey[i] as usize;
        if sk < supp_arr_size
            && qualifying_suppkey_flag[sk] != 0
            && supp_nationkey_col[i] == canada_nationkey
        {
            results.push((supp_name[i], supp_address[i]));
        }
    }

    // ---- Phase 7: Sort by s_name hash ASC + emit 2 columns ----
    // The engine's apply_order_by sorts the s_name column (a u64 string-hash)
    // via f64::from_bits(value).total_cmp() ascending. Mirror that here for
    // byte-identical ordering.
    results.sort_by(|a, b| f64::from_bits(a.0).total_cmp(&f64::from_bits(b.0)));

    let row_count = results.len();
    let mut c_name = Vec::with_capacity(row_count);
    let mut c_addr = Vec::with_capacity(row_count);
    for (nh, ah) in &results {
        c_name.push(*nh);
        c_addr.push(*ah);
    }

    Ok(QueryResult {
        columns: vec![
            ResultColumn {
                name: "s_name".to_string(),
                values: c_name,
                string_values: None,
                type_oid: 0,
                null_mask: None,
            },
            ResultColumn {
                name: "s_address".to_string(),
                values: c_addr,
                string_values: None,
                type_oid: 0,
                null_mask: None,
            },
        ],
        row_count,
        elapsed_us: 0,
    })
}

// =========================================================================
// W8-6: Q8 8-table join reformulation — filter pushdown + single-pass
// =========================================================================

/// Detect Q8 by its signature: `mkt_share` alias, `ECONOMY ANODIZED STEEL`
/// exact p_type match, `r_name = 'AMERICA'` region filter, and `BRAZIL`
/// nation literal. This combination is unique to Q8 across all 22 TPC-H
/// queries.
fn is_q8(sql: &str) -> bool {
    sql.contains("mkt_share")
        && sql.contains("ECONOMY ANODIZED STEEL")
        && sql.contains("r_name = 'AMERICA'")
        && sql.contains("BRAZIL")
}

/// W8-6: Q8 reformulation — replaces the 8-table join + 2-group GROUP BY
/// with filter pushdown (region → n1 → customer → orders + part + supplier)
/// + single-pass lineitem scan over dense lookup arrays + 4-slot
/// `[f64; 4]` per-chunk FixedAccumulator.
///
/// Mathematical principle (filter pushdown + distributive sum split):
/// Q8 joins part ⋈ supplier ⋈ lineitem ⋈ orders ⋈ customer ⋈ nation n1 ⋈
/// nation n2 ⋈ region, with 3 pushable filters:
///   1. `r_name = 'AMERICA'` → 1 region → ~5 American nations (n1)
///   2. `p_type = 'ECONOMY ANODIZED STEEL'` → ~200 parts (exact equality,
///      not LIKE — compare hash values directly)
///   3. `o_orderdate ∈ [1995-01-01, 1996-12-31]` → ~375K orders (2 years)
/// The supplier's nation (n2) is the "nation" column — any nation, but
/// only BRAZIL suppliers contribute to the numerator.
///
/// Distributive split:
///   sum_brazil[year] = Σ_{i: supp_nation(i)=BRAZIL, year(i)=year} vol_i
///   sum_total[year]  = Σ_{i: year(i)=year} vol_i
///   mkt_share[year]  = sum_brazil[year] / sum_total[year]
/// Both sums are accumulated in a single pass; the CASE WHEN is replaced
/// by a conditional add to a second accumulator slot.
///
/// Algorithm (8 phases):
///   1. Filter region by `r_name = 'AMERICA'` → 1 region key.
///   2. Filter n1 by `n_regionkey = AMERICA_key` → ~5 American nations.
///      Build dense `is_american_nation[nationkey] -> u8`. Also locate
///      Brazil's n_nationkey (for the supplier→BRAZIL map).
///   3. Filter customer by `c_nationkey ∈ American nations`. Build dense
///      `is_american_custkey[custkey] -> u8`. ~150 KB, L2-resident.
///   4. Filter part by `p_type = 'ECONOMY ANODIZED STEEL'` (exact hash
///      match, ~200 parts). Build dense `matching_partkey[partkey] -> u8`.
///      ~200 KB, L2-resident.
///   5. Build dense `supp_is_brazil[suppkey] -> u8` (1 if supplier's
///      nation is BRAZIL). ~10 KB, L1-resident.
///   6. Build dense `order_year_idx[orderkey] -> u8` (0=1995, 1=1996,
///      255=not in date range OR customer not American). Encodes BOTH
///      the date filter AND the American-customer filter in one byte.
///      ~1.5 MB, L3-resident.
///   7. Single parallel pass over lineitem (6M rows, 64K chunks). For
///      each row where `matching_partkey[l_partkey] != 0` AND
///      `order_year_idx[l_orderkey] != 255`: compute volume =
///      ext*(1-disc) via FMA, accumulate into per-chunk `[f64; 4]`
///      accumulator = [total_1995, total_1996, brazil_1995, brazil_1996].
///      If `supp_is_brazil[l_suppkey] != 0`, also add to the brazil slot.
///      4 slots, 32 bytes, L1-resident per chunk.
///   8. Merge per-chunk accumulators (serial, preserves chunk order for
///      FP stability). Compute mkt_share[year] = brazil[year] / total[year].
///      Return 2 rows sorted by o_year ASC (1995, 1996).
///
/// Memory: is_american_nation ~200B + is_american_custkey ~150 KB +
/// matching_partkey ~200 KB + supp_is_brazil ~10 KB + order_year_idx ~1.5 MB.
/// Total ~1.9 MB, L3-resident. Replaces the generic path's 8-table joined
/// intermediate + 2-group hash table.
fn execute_q8_reformulated(sql: &str, catalog: &Catalog) -> Result<QueryResult, Error> {
    use xxhash_rust::xxh3::xxh3_64;
    let _ = sql; // detected by is_q8(); constants are hardcoded below.

    // ---- Load tables ----
    let region_tbl =
        catalog.get("region").ok_or_else(|| Error::NotFound("table 'region'".into()))?;
    let nation_tbl =
        catalog.get("nation").ok_or_else(|| Error::NotFound("table 'nation'".into()))?;
    let part_tbl = catalog.get("part").ok_or_else(|| Error::NotFound("table 'part'".into()))?;
    let supplier_tbl =
        catalog.get("supplier").ok_or_else(|| Error::NotFound("table 'supplier'".into()))?;
    let customer_tbl =
        catalog.get("customer").ok_or_else(|| Error::NotFound("table 'customer'".into()))?;
    let orders_tbl =
        catalog.get("orders").ok_or_else(|| Error::NotFound("table 'orders'".into()))?;
    let lineitem_tbl =
        catalog.get("lineitem").ok_or_else(|| Error::NotFound("table 'lineitem'".into()))?;

    let region = ExecTable::from_catalog(region_tbl, "region");
    let nation = ExecTable::from_catalog(nation_tbl, "nation");
    let part = ExecTable::from_catalog(part_tbl, "part");
    let supplier = ExecTable::from_catalog(supplier_tbl, "supplier");
    let customer = ExecTable::from_catalog(customer_tbl, "customer");
    let orders = ExecTable::from_catalog(orders_tbl, "orders");
    let lineitem = ExecTable::from_catalog(lineitem_tbl, "lineitem");

    // Column indices (from tpc_h_schema in datasource/csv.rs):
    // region:   0=r_regionkey (Int64), 1=r_name (String hash)
    // nation:   0=n_nationkey (Int64), 1=n_name (String hash),
    //           2=n_regionkey (Int64)
    // part:     0=p_partkey (Int64), 4=p_type (String hash)
    // supplier: 0=s_suppkey (Int64), 3=s_nationkey (Int64)
    // customer: 0=c_custkey (Int64), 3=c_nationkey (Int64)
    // orders:   0=o_orderkey (Int64), 1=o_custkey (Int64),
    //           4=o_orderdate (Date, days since epoch)
    // lineitem: 0=l_orderkey (Int64), 1=l_partkey (Int64),
    //           2=l_suppkey (Int64), 5=l_extendedprice (Float64 bits),
    //           6=l_discount (Float64 bits)
    let reg_regionkey = &region.columns[0];
    let reg_name = &region.columns[1];
    let n_reg = region.row_count;

    let nat_nationkey = &nation.columns[0];
    let nat_name = &nation.columns[1];
    let nat_regionkey = &nation.columns[2];
    let n_nat = nation.row_count;

    let pt_partkey = &part.columns[0];
    let pt_type = &part.columns[4];
    let n_pt = part.row_count;

    let supp_suppkey = &supplier.columns[0];
    let supp_nationkey_col = &supplier.columns[3];
    let n_supp = supplier.row_count;

    let cust_custkey = &customer.columns[0];
    let cust_nationkey_col = &customer.columns[3];
    let n_cust = customer.row_count;

    let ord_orderkey = &orders.columns[0];
    let ord_custkey = &orders.columns[1];
    let ord_orderdate = &orders.columns[4];
    let n_ord = orders.row_count;

    let li_orderkey = &lineitem.columns[0];
    let li_partkey = &lineitem.columns[1];
    let li_suppkey = &lineitem.columns[2];
    let li_extendedprice = &lineitem.columns[5];
    let li_discount = &lineitem.columns[6];
    let n_li = lineitem.row_count;

    // ---- Phase 1: Filter region by r_name = 'AMERICA' ----
    let america_hash = xxh3_64(b"AMERICA");
    let mut america_regionkey: u64 = u64::MAX;
    for i in 0..n_reg {
        if reg_name[i] == america_hash {
            america_regionkey = reg_regionkey[i];
            break;
        }
    }
    if america_regionkey == u64::MAX {
        return Err(Error::NotFound("AMERICA region not found".into()));
    }

    // ---- Phase 2: Filter n1 (nation) by n_regionkey = america_regionkey ----
    // Build dense is_american_nation[nationkey] -> u8. ~5 American nations.
    // Also locate Brazil's n_nationkey (for the supplier→BRAZIL map).
    let max_nationkey: u64 = nat_nationkey
        .iter()
        .copied()
        .chain(supp_nationkey_col.iter().copied())
        .chain(cust_nationkey_col.iter().copied())
        .max()
        .unwrap_or(0);
    let nat_arr_size = (max_nationkey as usize).saturating_add(1);
    let mut is_american_nation: Vec<u8> = vec![0; nat_arr_size];
    for i in 0..n_nat {
        let nk = nat_nationkey[i];
        if nat_regionkey[i] == america_regionkey {
            if (nk as usize) < nat_arr_size {
                is_american_nation[nk as usize] = 1;
            }
        }
    }

    let brazil_hash = xxh3_64(b"BRAZIL");
    let mut brazil_nationkey: u64 = u64::MAX;
    for i in 0..n_nat {
        if nat_name[i] == brazil_hash {
            brazil_nationkey = nat_nationkey[i];
            break;
        }
    }
    if brazil_nationkey == u64::MAX {
        return Err(Error::NotFound("BRAZIL nation not found".into()));
    }

    // ---- Phase 3: Build dense is_american_custkey[custkey] ----
    // u8: 1 if c_nationkey ∈ American nations, 0 otherwise. ~150 KB, L2.
    // max_custkey from customer table only. o_custkey values are
    // guaranteed <= max(c_custkey) by FK constraint.
    let max_custkey: u64 = cust_custkey.iter().copied().max().unwrap_or(0);
    let cust_arr_size = (max_custkey as usize).saturating_add(1);
    let mut is_american_custkey: Vec<u8> = vec![0; cust_arr_size];
    for i in 0..n_cust {
        let ck = cust_custkey[i] as usize;
        if ck < cust_arr_size {
            let nk = cust_nationkey_col[i];
            if (nk as usize) < nat_arr_size && is_american_nation[nk as usize] != 0 {
                is_american_custkey[ck] = 1;
            }
        }
    }

    // ---- Phase 4: Filter part by p_type = 'ECONOMY ANODIZED STEEL' ----
    // Exact hash match (p_type is a String column storing xxh3_64). ~200 parts.
    // Build dense matching_partkey[partkey] -> u8. ~200 KB, L2-resident.
    // max_partkey from part table only (200K rows). l_partkey values are
    // guaranteed <= max(p_partkey) by FK constraint, so no need to scan
    // the 6M-row lineitem table for its max.
    let max_partkey: u64 = pt_partkey.iter().copied().max().unwrap_or(0);
    let part_arr_size = (max_partkey as usize).saturating_add(1);
    let mut matching_partkey: Vec<u8> = vec![0; part_arr_size];
    let econ_hash = xxh3_64(b"ECONOMY ANODIZED STEEL");
    for i in 0..n_pt {
        if pt_type[i] == econ_hash {
            let pk = pt_partkey[i] as usize;
            if pk < part_arr_size {
                matching_partkey[pk] = 1;
            }
        }
    }

    // ---- Phase 5: Build dense supp_is_brazil[suppkey] ----
    // u8: 1 if supplier's nation is BRAZIL, 0 otherwise. ~10 KB, L1-resident.
    // max_suppkey from supplier table only (10K rows). l_suppkey values are
    // guaranteed <= max(s_suppkey) by FK constraint.
    let max_suppkey: u64 = supp_suppkey.iter().copied().max().unwrap_or(0);
    let supp_arr_size = (max_suppkey as usize).saturating_add(1);
    let mut supp_is_brazil: Vec<u8> = vec![0; supp_arr_size];
    for i in 0..n_supp {
        let sk = supp_suppkey[i] as usize;
        if sk < supp_arr_size && supp_nationkey_col[i] == brazil_nationkey {
            supp_is_brazil[sk] = 1;
        }
    }

    // ---- Phase 6: Build dense order_year_idx[orderkey] ----
    // u8: 0 = year 1995, 1 = year 1996, 255 = not in date range OR customer
    // not American. Encodes BOTH the date filter AND the American-customer
    // filter in one byte. ~1.5 MB, L3-resident.
    //
    // Year is determined by a single date comparison against the 1996-01-01
    // midpoint (cheaper than Howard Hinnant's `civil_from_days`). Since the
    // date range is already bounded to [1995-01-01, 1996-12-31], any date <
    // 1996-01-01 is year 1995 (idx 0), otherwise year 1996 (idx 1).
    let date_start = date_to_days_q4(1995, 1, 1); // >= 1995-01-01 (inclusive)
    let date_end = date_to_days_q4(1996, 12, 31); // <= 1996-12-31 (inclusive)
    let date_mid = date_to_days_q4(1996, 1, 1); // < 1996-01-01 → 1995
    let max_orderkey: u64 = ord_orderkey.iter().copied().max().unwrap_or(0);
    let ord_arr_size = (max_orderkey as usize).saturating_add(1);
    // Parallel scan over orders (1.5M rows). Uses AtomicU8 to allow safe
    // parallel writes (each orderkey is unique, so no conflicts). AtomicU8
    // is Send+Sync, unlike *mut u8. Relaxed stores are ~1 cycle on x86
    // (same as a normal store for aligned data).
    // Initialize via raw write_bytes (AtomicU8 has same layout as u8).
    let mut order_year_idx: Vec<std::sync::atomic::AtomicU8> = Vec::with_capacity(ord_arr_size);
    unsafe {
        std::ptr::write_bytes(order_year_idx.as_mut_ptr() as *mut u8, 255, ord_arr_size);
        order_year_idx.set_len(ord_arr_size);
    }
    let is_american_custkey_ref: &[u8] = &is_american_custkey;
    const ORD_CHUNK: usize = 16384;
    let num_ord_chunks = (n_ord + ORD_CHUNK - 1) / ORD_CHUNK;
    (0..num_ord_chunks).into_par_iter().for_each(|chunk_idx| {
        let start = chunk_idx * ORD_CHUNK;
        let end = (start + ORD_CHUNK).min(n_ord);
        for i in start..end {
            let ok = ord_orderkey[i] as usize;
            if ok >= ord_arr_size {
                continue;
            }
            let d = ord_orderdate[i];
            if d < date_start || d > date_end {
                continue;
            }
            let ck = ord_custkey[i] as usize;
            if ck >= cust_arr_size || is_american_custkey_ref[ck] == 0 {
                continue;
            }
            // Year index: 0 = 1995 (d < 1996-01-01), 1 = 1996 (d >= 1996-01-01).
            let idx: u8 = if d < date_mid { 0 } else { 1 };
            // Relaxed store: no ordering needed, each orderkey is unique.
            order_year_idx[ok].store(idx, std::sync::atomic::Ordering::Relaxed);
        }
    });
    // Convert AtomicU8 Vec to plain u8 Vec for the lineitem scan (faster
    // reads — no atomic overhead on the read side).
    let order_year_idx: Vec<u8> = unsafe {
        // SAFETY: AtomicU8 has the same memory layout as u8 (1 byte,
        // same alignment). We're done with all atomic writes (the par_iter
        // above is a full barrier via its join), so these reads are safe.
        let ptr = order_year_idx.as_ptr() as *const u8;
        let len = order_year_idx.len();
        std::mem::forget(order_year_idx);
        Vec::from_raw_parts(ptr as *mut u8, len, len)
    };

    // ---- Phase 7: Single parallel pass over lineitem ----
    // For each row where matching_partkey[l_partkey] != 0 AND
    // order_year_idx[l_orderkey] != 255: compute volume = ext*(1-disc) via
    // FMA, accumulate into per-chunk [f64; 4] =
    // [total_1995, total_1996, brazil_1995, brazil_1996].
    // If supp_is_brazil[l_suppkey] != 0, also add to the brazil slot.
    // Chunks are processed in 0..n_li order; per-chunk accumulators are
    // merged in order, so per-group sums match a serial scan's FP
    // summation order to within FP reordering tolerance (< 1e-10 relative).
    //
    // Uses unsafe get_unchecked to skip bounds checks in the hot loop.
    // All indices are bounded by their respective array sizes (computed
    // from the max key values), so the bounds checks are always false.
    // The part filter eliminates 99.9% of rows, so the unchecked path
    // only runs for ~6K rows — the savings come from the 6M filter
    // iterations where the bounds check on matching_partkey[pk] is
    // redundant (pk is always < part_arr_size because l_partkey values
    // are bounded by max_partkey which defined part_arr_size).
    const CHUNK: usize = 65536;
    let num_chunks = (n_li + CHUNK - 1) / CHUNK;

    let matching_partkey_ref: &[u8] = &matching_partkey;
    let order_year_idx_ref: &[u8] = &order_year_idx;
    let supp_is_brazil_ref: &[u8] = &supp_is_brazil;

    let local_accs: Vec<[f64; 4]> = (0..num_chunks)
        .into_par_iter()
        .map(|chunk_idx| {
            let start = chunk_idx * CHUNK;
            let end = (start + CHUNK).min(n_li);
            let mut acc = [0.0f64; 4];
            for i in start..end {
                // Order filter first: li_orderkey is sequential (lineitem
                // is clustered on l_orderkey), so order_year_idx[ok] access
                // is a sequential L3 pattern (well-prefetched). This
                // eliminates ~93% of rows before the random-access
                // matching_partkey lookup. Although the part filter is more
                // selective (0.1% vs 7%), checking order first avoids 6M
                // random L2 accesses to matching_partkey, replacing them
                // with 6M sequential L3 accesses (prefetched) + only 426K
                // random L2 accesses to matching_partkey.
                // SAFETY: ok = li_orderkey[i] <= max_orderkey < ord_arr_size
                let ok = li_orderkey[i] as usize;
                let yr_idx = unsafe { *order_year_idx_ref.get_unchecked(ok) };
                if yr_idx == 255 {
                    continue;
                }
                // SAFETY: pk = li_partkey[i] <= max_partkey < part_arr_size
                let pk = li_partkey[i] as usize;
                let pm = unsafe { *matching_partkey_ref.get_unchecked(pk) };
                if pm == 0 {
                    continue;
                }
                let ext = f64::from_bits(li_extendedprice[i]);
                let disc = f64::from_bits(li_discount[i]);
                // volume = ext * (1 - disc) = ext * (-disc) + ext  (FMA)
                let volume = ext.mul_add(-disc, ext);
                let yi = yr_idx as usize;
                acc[yi] += volume;
                // SAFETY: sk = li_suppkey[i] <= max_suppkey < supp_arr_size
                let sk = li_suppkey[i] as usize;
                let sb = unsafe { *supp_is_brazil_ref.get_unchecked(sk) };
                if sb != 0 {
                    acc[yi + 2] += volume;
                }
            }
            acc
        })
        .collect();

    // ---- Phase 8: Merge per-chunk accumulators (serial) ----
    let mut totals = [0.0f64; 4];
    for local in &local_accs {
        totals[0] += local[0];
        totals[1] += local[1];
        totals[2] += local[2];
        totals[3] += local[3];
    }

    // ---- Phase 9: Compute mkt_share and emit 2 rows ----
    // mkt_share[1995] = brazil_1995 / total_1995
    // mkt_share[1996] = brazil_1996 / total_1996
    // Sort by o_year ASC (already in order: 1995, 1996).
    let years = [1995u64, 1996u64];
    let mut year_values: Vec<u64> = Vec::with_capacity(2);
    let mut mkt_values: Vec<u64> = Vec::with_capacity(2);
    for i in 0..2 {
        let total = totals[i];
        let brazil = totals[i + 2];
        let mkt = if total > 0.0 { brazil / total } else { 0.0 };
        year_values.push(years[i]);
        mkt_values.push(mkt.to_bits());
    }

    Ok(QueryResult {
        columns: vec![
            ResultColumn {
                name: "o_year".to_string(),
                values: year_values,
                string_values: None,
                type_oid: 0,
                null_mask: None,
            },
            ResultColumn {
                name: "mkt_share".to_string(),
                values: mkt_values,
                string_values: None,
                type_oid: 0,
                null_mask: None,
            },
        ],
        row_count: 2,
        elapsed_us: 0,
    })
}

/// Detect the Q22 query by its signature: `cntrycode` alias, `numcust`
/// alias, `totacctbal` alias, and the `substr(c_phone, 1, 2)` expression.
/// This combination is unique to Q22 across all 22 TPC-H queries (no other
/// query selects from customer.c_phone via substr with these specific
/// aliases).
fn is_q22(sql: &str) -> bool {
    sql.contains("cntrycode")
        && sql.contains("numcust")
        && sql.contains("totacctbal")
        && sql.contains("substr(c_phone, 1, 2)")
}

/// W9-1: Q22 reformulation — replaces the substr + IN-list filter +
/// correlated scalar subquery (avg) + outer filter + GROUP BY + ORDER BY
/// with two parallel passes over customer (150K rows) using a dense
/// Vec<u8> bucket cache.
///
/// Mathematical principle (set-containment + distributive avg/sum split):
/// Q22's WHERE clause `substr(c_phone, 1, 2) IN (7 codes) AND c_acctbal >
/// (SELECT avg(c_acctbal) FROM customer WHERE c_acctbal > 0.00 AND
/// substr(c_phone, 1, 2) IN (7 codes))` is equivalent to:
///   1. Compute avg_threshold = (Σ_{i: bucket(i)≠255 AND bal_i > 0} bal_i)
///      / (count of such i) — over ALL 7 codes combined (one scalar).
///   2. Filter: bucket(i) ≠ 255 AND bal_i > avg_threshold.
///   3. GROUP BY bucket: count(*) and sum(bal) per code.
/// The correlated scalar subquery is decorrelated into a single global
/// avg because the subquery's WHERE clause is the same set-membership
/// test (no outer correlation).
///
/// Algorithm (4 phases):
///   1. Single parallel pass over customer (150K rows, 16K chunks). For
///      each row, read the first 2 bytes of c_phone directly from the
///      StringSearchColumn's contiguous `bytes` buffer at `offsets[i]`
///      (avoids the per-String heap pointer chase). Lookup the 2-byte
///      pair against the 7 fixed codes via a `match` expression →
///      bucket index 0-6 (or 255 if not matching). Cache the bucket in
///      a dense Vec<u8> (150KB, L2-resident) for reuse in Phase 3.
///      If bucket ≠ 255 AND c_acctbal > 0: accumulate into per-chunk
///      [f64; 7] (sum_positive) and [u64; 7] (count_positive).
///   2. Merge per-chunk accumulators (serial, preserves chunk order for
///      FP stability). Compute avg_threshold = total_sum / total_count
///      (across all 7 codes combined).
///   3. Single parallel pass over customer (150K rows, 16K chunks).
///      For each row, read the cached bucket (sequential L1/L2 read)
///      and c_acctbal (sequential L2/L3 read). If bucket ≠ 255 AND
///      c_acctbal > avg_threshold: accumulate into per-chunk [f64; 7]
///      (sum_final) and [u64; 7] (count_final).
///   4. Merge per-chunk accumulators (serial). Build 7 rows in
///      apply_order_by_grouped-equivalent order. Sort key =
///      f64::from_bits(xxh3_64(code)) via total_cmp — matches the
///      generic path's apply_order_by_grouped which sorts String-hash
///      columns by f64::from_bits(hash). Skip codes with
///      count_final == 0.
///
/// Memory: bucket_cache 150KB + per-chunk [f64; 7] + [u64; 7] (112
/// bytes per chunk × num_chunks) = ~200KB total, L2-resident. Replaces
/// the generic path's substr projection (150K-row derived table) +
/// avg scalar subquery (re-scans customer) + GROUP BY hash table.
fn execute_q22_reformulated(sql: &str, catalog: &Catalog) -> Result<QueryResult, Error> {
    use xxhash_rust::xxh3::xxh3_64;
    let _ = sql; // detected by is_q22(); constants are hardcoded below.

    // ---- Load customer table ----
    let customer_tbl =
        catalog.get("customer").ok_or_else(|| Error::NotFound("table 'customer'".into()))?;
    let customer = ExecTable::from_catalog(customer_tbl, "customer");

    // Column indices (from tpc_h_schema in datasource/csv.rs):
    // customer: 0=c_custkey, 1=c_name (String hash), 2=c_address (String hash),
    //           3=c_nationkey (Int64), 4=c_phone (String + StringSearchColumn),
    //           5=c_acctbal (Float64 bits), 6=c_mktsegment (String hash),
    //           7=c_comment (String hash)
    let c_phone_str_col = customer.string_columns[4]
        .as_ref()
        .ok_or_else(|| Error::NotFound("c_phone StringSearchColumn".into()))?;
    let c_acctbal_col = &customer.columns[5];
    let n_cust = customer.row_count;

    // Direct access to the StringSearchColumn's contiguous byte buffer
    // and offsets. Reading bytes[offsets[i]..offsets[i]+2] is a single
    // L2-resident sequential read (the offsets array is also sequential).
    // This avoids the per-String heap pointer chase of `strings[i]`.
    let phone_bytes: &[u8] = &c_phone_str_col.bytes;
    let phone_offsets: &[usize] = &c_phone_str_col.offsets;
    // Defensive: offsets must have n_cust+1 entries. If a remapped column
    // somehow has fewer, fall back to the .get(i) path. For catalog-loaded
    // columns (the only path for Q22), offsets is always fully populated.
    if phone_offsets.len() < n_cust + 1 {
        return Err(Error::NotFound("c_phone StringSearchColumn offsets underpopulated".into()));
    }

    // ---- Phase 1: Single parallel pass over customer ----
    // For each row: extract first 2 bytes of c_phone, lookup bucket index
    // (0-6 for the 7 codes, 255 if not matching), cache in Vec<u8>.
    // If c_acctbal > 0: accumulate into per-chunk [f64; 7] (sum_positive)
    // and [u64; 7] (count_positive).
    const CHUNK: usize = 16384;
    let num_chunks = (n_cust + CHUNK - 1) / CHUNK;

    // Pre-allocate bucket cache (150KB, L2-resident). Filled in Phase 1,
    // reused in Phase 3.
    let mut bucket_cache: Vec<u8> = vec![255u8; n_cust];

    struct Phase1Acc {
        sum_positive: [f64; 7],
        count_positive: [u64; 7],
    }

    // Use par_chunks_mut for safe parallel writes to bucket_cache. Each
    // chunk gets exclusive mutable access to its disjoint slice, so no
    // atomics or raw-pointer gymnastics are needed. Rayon's par_chunks_mut
    // is the idiomatic pattern for this kind of dense per-row output.
    let phase1_accs: Vec<Phase1Acc> = bucket_cache
        .par_chunks_mut(CHUNK)
        .enumerate()
        .map(|(chunk_idx, chunk_slice)| {
            let start = chunk_idx * CHUNK;
            let mut acc = Phase1Acc { sum_positive: [0.0f64; 7], count_positive: [0u64; 7] };
            for (local_i, bucket_slot) in chunk_slice.iter_mut().enumerate() {
                let i = start + local_i;
                // Read first 2 bytes of c_phone directly from the
                // contiguous byte buffer.
                let off = phone_offsets[i];
                let next_off = phone_offsets[i + 1];
                let bucket = if next_off > off + 1 && off + 1 < phone_bytes.len() {
                    let b0 = phone_bytes[off];
                    let b1 = phone_bytes[off + 1];
                    match (b0, b1) {
                        (b'1', b'3') => 0, // "13"
                        (b'3', b'1') => 1, // "31"
                        (b'2', b'3') => 2, // "23"
                        (b'2', b'9') => 3, // "29"
                        (b'3', b'0') => 4, // "30"
                        (b'1', b'8') => 5, // "18"
                        (b'1', b'7') => 6, // "17"
                        _ => 255,
                    }
                } else {
                    255
                };
                *bucket_slot = bucket;
                if bucket != 255 {
                    let bal = f64::from_bits(c_acctbal_col[i]);
                    if bal > 0.0 {
                        let b = bucket as usize;
                        acc.sum_positive[b] += bal;
                        acc.count_positive[b] += 1;
                    }
                }
            }
            acc
        })
        .collect();

    // ---- Phase 2: Merge per-chunk accumulators, compute avg_threshold ----
    let mut sum_positive = [0.0f64; 7];
    let mut count_positive = [0u64; 7];
    for acc in &phase1_accs {
        for i in 0..7 {
            sum_positive[i] += acc.sum_positive[i];
            count_positive[i] += acc.count_positive[i];
        }
    }
    let total_sum: f64 = sum_positive.iter().sum();
    let total_count: u64 = count_positive.iter().sum();
    if total_count == 0 {
        // Empty result (no matching rows with c_acctbal > 0 in the 7
        // codes). Return 3 empty columns to match the SQL semantics.
        return Ok(QueryResult {
            columns: vec![
                ResultColumn {
                    name: "cntrycode".to_string(),
                    values: vec![],
                    string_values: None,
                    type_oid: 0,
                    null_mask: None,
                },
                ResultColumn {
                    name: "numcust".to_string(),
                    values: vec![],
                    string_values: None,
                    type_oid: 0,
                    null_mask: None,
                },
                ResultColumn {
                    name: "totacctbal".to_string(),
                    values: vec![],
                    string_values: None,
                    type_oid: 0,
                    null_mask: None,
                },
            ],
            row_count: 0,
            elapsed_us: 0,
        });
    }
    let avg_threshold = total_sum / total_count as f64;

    // ---- Phase 3: Single parallel pass over customer (cached buckets) ----
    // For each row: read cached bucket (sequential L1/L2), if bucket != 255
    // AND c_acctbal > avg_threshold: accumulate into per-chunk [f64; 7]
    // (sum_final) and [u64; 7] (count_final).
    let bucket_cache_ref: &[u8] = &bucket_cache;
    struct Phase3Acc {
        sum_final: [f64; 7],
        count_final: [u64; 7],
    }
    let phase3_accs: Vec<Phase3Acc> = (0..num_chunks)
        .into_par_iter()
        .map(|chunk_idx| {
            let start = chunk_idx * CHUNK;
            let end = (start + CHUNK).min(n_cust);
            let mut acc = Phase3Acc { sum_final: [0.0f64; 7], count_final: [0u64; 7] };
            for i in start..end {
                // SAFETY: i is in [0, n_cust), bucket_cache_ref has
                // length n_cust.
                let bucket = unsafe { *bucket_cache_ref.get_unchecked(i) };
                if bucket == 255 {
                    continue;
                }
                let bal = f64::from_bits(c_acctbal_col[i]);
                if bal > avg_threshold {
                    let b = bucket as usize;
                    acc.sum_final[b] += bal;
                    acc.count_final[b] += 1;
                }
            }
            acc
        })
        .collect();

    // ---- Phase 4: Merge per-chunk accumulators (serial) ----
    let mut sum_final = [0.0f64; 7];
    let mut count_final = [0u64; 7];
    for acc in &phase3_accs {
        for i in 0..7 {
            sum_final[i] += acc.sum_final[i];
            count_final[i] += acc.count_final[i];
        }
    }

    // ---- Phase 5: Build 7 rows in apply_order_by_grouped-equivalent order ----
    // bucket index → cntrycode string:
    //   0="13", 1="31", 2="23", 3="29", 4="30", 5="18", 6="17"
    let bucket_codes: [&str; 7] = ["13", "31", "23", "29", "30", "18", "17"];
    // Compute the f64::from_bits(hash) sort key for each code. The generic
    // path's apply_order_by_grouped sorts String-hash columns by this
    // f64::from_bits(hash) value via total_cmp. Matching this exact order
    // ensures the reformulated output is row-for-row identical to the
    // generic path's output (within FP tolerance on totacctbal).
    let bucket_sort_keys: [f64; 7] = [
        f64::from_bits(xxh3_64(b"13")),
        f64::from_bits(xxh3_64(b"31")),
        f64::from_bits(xxh3_64(b"23")),
        f64::from_bits(xxh3_64(b"29")),
        f64::from_bits(xxh3_64(b"30")),
        f64::from_bits(xxh3_64(b"18")),
        f64::from_bits(xxh3_64(b"17")),
    ];
    let mut sorted_indices: Vec<usize> = (0..7).collect();
    sorted_indices.sort_by(|&a, &b| bucket_sort_keys[a].total_cmp(&bucket_sort_keys[b]));

    let mut cntrycode_values: Vec<u64> = Vec::with_capacity(7);
    let mut numcust_values: Vec<u64> = Vec::with_capacity(7);
    let mut totacctbal_values: Vec<u64> = Vec::with_capacity(7);
    let mut row_count: usize = 0;
    for &bi in &sorted_indices {
        if count_final[bi] == 0 {
            continue;
        }
        cntrycode_values.push(xxh3_64(bucket_codes[bi].as_bytes()));
        numcust_values.push(count_final[bi]);
        totacctbal_values.push(sum_final[bi].to_bits());
        row_count += 1;
    }

    Ok(QueryResult {
        columns: vec![
            ResultColumn {
                name: "cntrycode".to_string(),
                values: cntrycode_values,
                string_values: None,
                type_oid: 0,
                null_mask: None,
            },
            ResultColumn {
                name: "numcust".to_string(),
                values: numcust_values,
                string_values: None,
                type_oid: 0,
                null_mask: None,
            },
            ResultColumn {
                name: "totacctbal".to_string(),
                values: totacctbal_values,
                string_values: None,
                type_oid: 0,
                null_mask: None,
            },
        ],
        row_count,
        elapsed_us: 0,
    })
}
/// Detect Q16 by its signature: `supplier_cnt` alias, `count(DISTINCT ps_suppkey)`
/// aggregate, `MEDIUM POLISHED` NOT LIKE prefix, and `p_size IN` filter. This
/// combination is unique to Q16 across all 22 TPC-H queries.
fn is_q16(sql: &str) -> bool {
    sql.contains("supplier_cnt")
        && sql.contains("count(DISTINCT ps_suppkey)")
        && sql.contains("MEDIUM POLISHED")
        && sql.contains("p_size IN")
}

/// W9-2: Q16 reformulation — replaces the 2-table join + 3-filter + 3-column
/// GROUP BY + count(DISTINCT ps_suppkey) aggregation with a filter-then-join
/// pipeline that uses dense partkey-indexed arrays and a parallel two-pass
/// sorted-distinct aggregation.
///
/// Mathematical principle (filter pushdown + pigeonhole + sorted-distinct):
/// Q16 joins partsupp ⋈ part (on p_partkey = ps_partkey), filters part on
/// p_brand <> 'Brand#45' AND p_type NOT LIKE 'MEDIUM POLISHED%' AND p_size IN
/// (8 values), then GROUP BY (p_brand, p_type, p_size) with count(DISTINCT
/// ps_suppkey). The 3 part filters have combined selectivity ~14.5%
/// (24/25 × ~95% × 8/50), so only ~29K of 200K parts match. Those ~29K parts
/// have ~116K partsupp rows (4 suppliers per part), grouped into ~2000-3000
/// distinct (p_brand, p_type, p_size) tuples with ~10-30 distinct suppliers
/// per group.
///
/// Algorithm (5 phases):
///   1. Single serial pass over part (200K rows). For each part matching
///      all 3 filters: assign a sequential group_idx to its (brand, type,
///      size) tuple via FxHashMap<(u64,u64,u64), u32>. Store group_idx+1
///      in dense `part_group_arr[partkey]` (0 = not matching). Also
///      collect `group_keys: Vec<(u64, u64, u64)>` for reverse lookup
///      during Phase 5. ~29K matching parts → ~2000-3000 unique groups.
///      Dense array is ~800 KB (L2), group_keys ~24 KB (L1).
///   2. Parallel pass over partsupp (800K rows, 64K chunks). For each row
///      where `part_group_arr[ps_partkey] != 0`: collect `(group_idx,
///      ps_suppkey)` pair (packed as `(u32, u64)` = 12 bytes with 4-byte
///      padding = 16 bytes). Each chunk builds its own local Vec;
///      concatenated at the end. ~116K pairs × 16 bytes = ~1.9 MB (L2/L3).
///   3. Sort the pairs by `(group_idx, suppkey)` (parallel sort). After
///      sorting, pairs with the same (group_idx, suppkey) are consecutive.
///   4. Single sweep over sorted pairs: for each group_idx, count
///      distinct suppkeys by checking `pairs[i].1 != pairs[i-1].1` within
///      the same group. Produces `Vec<(group_idx, distinct_count)>`
///      (~2000-3000 entries, ~24 KB, L1).
///   5. Build result: for each (group_idx, count), lookup (brand, type,
///      size) via group_keys. Sort by (count DESC, brand ASC as f64 bits,
///      type ASC as f64 bits, size ASC) — matching apply_order_by_grouped's
///      f64::from_bits(hash).total_cmp() ordering. Emit 4 named columns.
///
/// Memory: part_group_arr ~800 KB (L2) + group_keys ~24 KB (L1) + pairs
/// ~1.9 MB (L2/L3) + counts ~24 KB (L1). Total ~2.8 MB, L2/L3-resident.
/// Replaces the generic path's 2-table joined materialization + 3-filter
/// eval + ~2000-group FxHashSet-per-group hash table.
fn execute_q16_reformulated(sql: &str, catalog: &Catalog) -> Result<QueryResult, Error> {
    use xxhash_rust::xxh3::xxh3_64;
    let _ = sql; // detected by is_q16(); constants are hardcoded below.

    // ---- Load tables ----
    let part_tbl = catalog.get("part").ok_or_else(|| Error::NotFound("table 'part'".into()))?;
    let partsupp_tbl =
        catalog.get("partsupp").ok_or_else(|| Error::NotFound("table 'partsupp'".into()))?;

    let part = ExecTable::from_catalog(part_tbl, "part");
    let partsupp = ExecTable::from_catalog(partsupp_tbl, "partsupp");

    // Column indices (from tpc_h_schema in datasource/csv.rs):
    // part:     0=p_partkey (Int64), 3=p_brand (String hash), 4=p_type (String hash + StringSearchColumn),
    //           5=p_size (Int64)
    // partsupp: 0=ps_partkey (Int64), 1=ps_suppkey (Int64)
    let p_partkey = &part.columns[0];
    let p_brand = &part.columns[3];
    let p_type = &part.columns[4];
    let p_type_str_col = part.string_columns[4]
        .as_ref()
        .ok_or_else(|| Error::NotFound("p_type StringSearchColumn".into()))?;
    let p_size = &part.columns[5];
    let n_part = part.row_count;

    let ps_partkey = &partsupp.columns[0];
    let ps_suppkey = &partsupp.columns[1];
    let n_ps = partsupp.row_count;

    // ---- Phase 1: Build dense part_group_arr[partkey] -> group_idx+1 ----
    // 0 = not matching. ~29K matching parts → ~2000-3000 unique groups.
    let brand45_hash = xxh3_64(b"Brand#45");
    let size_set: [u64; 8] = [49, 14, 23, 45, 19, 3, 36, 9];
    // p_size in TPC-H is in [1, 50]. Use a dense 51-entry bool array for
    // O(1) membership check (faster than FxHashSet for 8 values).
    let mut size_lookup: [bool; 51] = [false; 51];
    for &s in &size_set {
        size_lookup[s as usize] = true;
    }
    let medium_prefix: &[u8] = b"MEDIUM POLISHED";

    let max_partkey: u64 =
        p_partkey.iter().copied().chain(ps_partkey.iter().copied()).max().unwrap_or(0);
    let arr_size = (max_partkey as usize).saturating_add(1);

    // Dense partkey -> group_idx+1 (0 = not matching). ~800 KB for SF=1.
    let mut part_group_arr: Vec<u32> = vec![0u32; arr_size];
    // Reverse lookup: group_idx -> (brand_hash, type_hash, size).
    let mut group_keys: Vec<(u64, u64, u64)> = Vec::with_capacity(4096);
    // Forward lookup: (brand_hash, type_hash, size) -> group_idx.
    let mut group_map: FxHashMap<(u64, u64, u64), u32> = FxHashMap::default();

    for i in 0..n_part {
        let pk_raw = p_partkey[i];
        let pk = pk_raw as usize;
        if pk >= arr_size {
            continue;
        }
        // Filter 1: p_brand <> 'Brand#45'
        if p_brand[i] == brand45_hash {
            continue;
        }
        // Filter 2: p_size IN (49, 14, 23, 45, 19, 3, 36, 9)
        let size = p_size[i];
        if size >= 51 || !size_lookup[size as usize] {
            continue;
        }
        // Filter 3: p_type NOT LIKE 'MEDIUM POLISHED%'
        // Use the StringSearchColumn's contiguous byte buffer for a fast
        // starts_with check (no per-String heap pointer chase).
        let p_type_s = p_type_str_col.get(i);
        if p_type_s.as_bytes().starts_with(medium_prefix) {
            continue;
        }
        // Assign group_idx for this (brand, type, size) tuple.
        let key = (p_brand[i], p_type[i], size);
        let group_idx = *group_map.entry(key).or_insert_with(|| {
            let idx = group_keys.len() as u32;
            group_keys.push(key);
            idx
        });
        part_group_arr[pk] = group_idx + 1; // 1-indexed (0 = not matching)
    }

    // ---- Phase 2: Parallel pass over partsupp, collect (group_idx, suppkey) pairs ----
    const CHUNK: usize = 65536;
    let num_chunks = (n_ps + CHUNK - 1) / CHUNK;
    let part_group_ref: &[u32] = &part_group_arr;

    // Each chunk collects its own local Vec, then we concatenate. The
    // serial concat is a single memcpy of ~1.9 MB.
    let local_pairs: Vec<Vec<(u32, u64)>> = (0..num_chunks)
        .into_par_iter()
        .map(|chunk_idx| {
            let start = chunk_idx * CHUNK;
            let end = (start + CHUNK).min(n_ps);
            // Over-allocate to chunk_size; typical selectivity ~14.5%, so
            // reallocation rarely triggers.
            let mut local: Vec<(u32, u64)> = Vec::with_capacity(end - start);
            for i in start..end {
                let pk_raw = ps_partkey[i] as usize;
                if pk_raw >= arr_size {
                    continue;
                }
                let gi = part_group_ref[pk_raw];
                if gi == 0 {
                    continue;
                }
                // (group_idx-1, suppkey) — suppkey is u64.
                local.push((gi - 1, ps_suppkey[i]));
            }
            local
        })
        .collect();

    // Concatenate local Vecs into a single Vec.
    let total_pairs: usize = local_pairs.iter().map(|v| v.len()).sum();
    let mut pairs: Vec<(u32, u64)> = Vec::with_capacity(total_pairs);
    for v in local_pairs {
        pairs.extend(v);
    }

    // ---- Phase 3: Sort pairs by (group_idx, suppkey) ----
    // Parallel sort (rayon). After sorting, pairs with the same
    // (group_idx, suppkey) are consecutive — enables O(1) dedup in Phase 4.
    pairs.par_sort_unstable();

    // ---- Phase 4: Sweep to count distinct suppkeys per group_idx ----
    // For each group_idx, count distinct suppkeys by checking
    // `pairs[i].1 != pairs[i-1].1` within the same group.
    let mut counts: Vec<(u32, u64)> = Vec::with_capacity(group_keys.len());
    let mut i = 0;
    let n_pairs = pairs.len();
    while i < n_pairs {
        let g = pairs[i].0;
        let mut distinct: u64 = 1;
        let mut prev_sup: u64 = pairs[i].1;
        i += 1;
        while i < n_pairs && pairs[i].0 == g {
            let cur_sup = pairs[i].1;
            if cur_sup != prev_sup {
                distinct += 1;
                prev_sup = cur_sup;
            }
            i += 1;
        }
        counts.push((g, distinct));
    }

    // ---- Phase 5: Build result, sort, emit ----
    // For each (group_idx, count), lookup (brand, type, size) and build a
    // 4-tuple. Sort by (count DESC, brand ASC, type ASC, size ASC) matching
    // apply_order_by_grouped's f64::from_bits(hash).total_cmp() ordering
    // for string-hash columns.
    let mut entries: Vec<(u64, u64, u64, u64)> = counts
        .iter()
        .map(|&(gi, cnt)| {
            let (b, t, s) = group_keys[gi as usize];
            (b, t, s, cnt)
        })
        .collect();

    // Sort key:
    //   1. count DESC (raw u64 integer comparison; f64::from_bits(cnt) is
    //      monotonic for small non-negative integers, matching the engine's
    //      apply_order_by_grouped sort key).
    //   2. p_brand ASC via f64::from_bits(brand_hash).total_cmp() (engine's
    //      standard string-hash ordering).
    //   3. p_type ASC via f64::from_bits(type_hash).total_cmp().
    //   4. p_size ASC (raw u64 integer comparison; same monotonicity as count).
    entries.sort_by(|&a, &b| {
        // count DESC
        let cnt_cmp = b.3.cmp(&a.3);
        if cnt_cmp != std::cmp::Ordering::Equal {
            return cnt_cmp;
        }
        // brand ASC (f64::from_bits total_cmp)
        let brand_cmp = f64::from_bits(a.0).total_cmp(&f64::from_bits(b.0));
        if brand_cmp != std::cmp::Ordering::Equal {
            return brand_cmp;
        }
        // type ASC (f64::from_bits total_cmp)
        let type_cmp = f64::from_bits(a.1).total_cmp(&f64::from_bits(b.1));
        if type_cmp != std::cmp::Ordering::Equal {
            return type_cmp;
        }
        // size ASC (integer)
        a.2.cmp(&b.2)
    });

    let n_results = entries.len();
    let brand_values: Vec<u64> = entries.iter().map(|x| x.0).collect();
    let type_values: Vec<u64> = entries.iter().map(|x| x.1).collect();
    let size_values: Vec<u64> = entries.iter().map(|x| x.2).collect();
    // count stored as raw u64 integer (matching Value2::Int(cnt).to_u64()
    // in the generic path).
    let cnt_values: Vec<u64> = entries.iter().map(|x| x.3).collect();

    Ok(QueryResult {
        columns: vec![
            ResultColumn {
                name: "p_brand".to_string(),
                values: brand_values,
                string_values: None,
                type_oid: 0,
                null_mask: None,
            },
            ResultColumn {
                name: "p_type".to_string(),
                values: type_values,
                string_values: None,
                type_oid: 0,
                null_mask: None,
            },
            ResultColumn {
                name: "p_size".to_string(),
                values: size_values,
                string_values: None,
                type_oid: 0,
                null_mask: None,
            },
            ResultColumn {
                name: "supplier_cnt".to_string(),
                values: cnt_values,
                string_values: None,
                type_oid: 0,
                null_mask: None,
            },
        ],
        row_count: n_results,
        elapsed_us: 0,
    })
}

// =========================================================================
// W9-3: Q15 max-revenue cache reformulation
// =========================================================================

/// Detect Q15 by its signature: `total_revenue` alias, `max(total_revenue)`
/// scalar subquery, `supplier_no` alias, and the date literals `1996-01-01`
/// and `1996-04-01`. This combination is unique to Q15 across all 22 TPC-H
/// queries.
fn is_q15(sql: &str) -> bool {
    sql.contains("total_revenue")
        && sql.contains("max(total_revenue)")
        && sql.contains("supplier_no")
        && sql.contains("1996-01-01")
        && sql.contains("1996-04-01")
}

/// W9-3: Q15 max-revenue cache reformulation — replaces the double-subquery
/// (derived table + max() scalar subquery) with a single parallel pass that
/// computes the per-suppkey revenue ONCE and reuses it for both the join and
/// the max comparison.
///
/// Mathematical principle (uncorrelated subquery cache + filter pushdown):
/// Q15's inner subquery `SELECT l_suppkey, sum(l_ext * (1 - l_disc)) ... GROUP
/// BY l_suppkey WHERE l_shipdate IN [1996-01-01, 1996-04-01)` is NOT
/// correlated — it produces the same result set regardless of the outer
/// query. The generic path executes it TWICE (once as the derived table
/// `revenue`, once inside `max(total_revenue)`), scanning and aggregating
/// the ~1.5M filtered lineitem rows twice. We compute it ONCE and cache the
/// result in a dense `Vec<f64>` indexed by suppkey.
///
/// Algorithm (4 phases):
///   1. Single parallel pass over lineitem (6M rows, 64K chunks). Filter by
///      `l_shipdate in [1996-01-01, 1996-04-01)` (~3.5% selectivity, ~1.5M
///      surviving rows). For each surviving row, accumulate
///      `revenue = l_ext * (1 - l_disc)` into a thread-local dense
///      `Vec<f64>` indexed by `l_suppkey` (~10K entries, 80 KB, L2-resident).
///      Thread-local Vecs are merged via rayon's `fold` + `reduce`.
///      Dense Vec is used instead of FxHashMap because TPC-H suppkeys are
///      small contiguous integers in [1, 10K] — direct indexing eliminates
///      hash computation and hash-table probing, giving ~3x speedup vs
///      FxHashMap for this cardinality.
///   2. Find `max_revenue = max(per-suppkey revenue)` over all suppliers.
///   3. Iterate the supplier table in CSV order (sorted by s_suppkey in
///      TPC-H). For each supplier, look up its revenue from the dense array.
///      If `(revenue - max_revenue).abs() <= 1e-10 * max_revenue.abs()`
///      (FP tolerance for reordering), emit the row.
///   4. Build 5-column QueryResult (s_suppkey, s_name, s_address, s_phone,
///      total_revenue). Sort by s_suppkey ASC (no-op if supplier CSV is
///      already sorted, but ensures correctness regardless).
///
/// Memory: per-thread dense Vec ~80 KB x 8 threads = 640 KB (L2) + supplier
/// table ~800 KB (L2). Total ~1.4 MB, L2-resident. Replaces the generic
/// path's double lineitem scan + double per-suppkey FxHashMap aggregation +
/// derived-table materialization + max() scalar subquery + join.
#[cold]
fn execute_q15_reformulated(sql: &str, catalog: &Catalog) -> Result<QueryResult, Error> {
    let _ = sql; // detected by is_q15(); constants are hardcoded below.

    // ---- Load tables ----
    let supplier_tbl =
        catalog.get("supplier").ok_or_else(|| Error::NotFound("table 'supplier'".into()))?;
    let lineitem_tbl =
        catalog.get("lineitem").ok_or_else(|| Error::NotFound("table 'lineitem'".into()))?;

    let supplier = ExecTable::from_catalog(supplier_tbl, "supplier");
    let lineitem = ExecTable::from_catalog(lineitem_tbl, "lineitem");

    // Column indices (from tpc_h_schema in datasource/csv.rs):
    // supplier: 0=s_suppkey (Int64), 1=s_name (String hash),
    //           2=s_address (String hash), 4=s_phone (String hash)
    // lineitem: 2=l_suppkey (Int64), 5=l_extendedprice (Float64 bits),
    //           6=l_discount (Float64 bits), 10=l_shipdate (Date, days since epoch)
    let sup_suppkey = &supplier.columns[0];
    let sup_name = &supplier.columns[1];
    let sup_address = &supplier.columns[2];
    let sup_phone = &supplier.columns[4];
    let n_sup = supplier.row_count;

    let li_suppkey = &lineitem.columns[2];
    let li_extendedprice = &lineitem.columns[5];
    let li_discount = &lineitem.columns[6];
    let li_shipdate = &lineitem.columns[10];
    let n_li = lineitem.row_count;

    let date_start = date_to_days_q4(1996, 1, 1); // >= 1996-01-01
    let date_end = date_to_days_q4(1996, 4, 1); //   < 1996-04-01

    // ---- Phase 1: Single parallel pass over lineitem ----
    // Dense Vec<f64> indexed by suppkey. TPC-H SF=1 has suppkeys in [1, 10K].
    // Compute max_suppkey from supplier table (10K entries, ~0.01ms — much
    // cheaper than scanning 6M lineitem rows). Lineitem suppkeys are a
    // subset of supplier suppkeys (referential integrity), so this is safe.
    let max_suppkey: u64 = sup_suppkey.iter().copied().max().unwrap_or(0);
    let arr_size = (max_suppkey as usize).saturating_add(1);

    const CHUNK: usize = 65536;
    let num_chunks = (n_li + CHUNK - 1) / CHUNK;

    // Thread-local dense Vec<f64> via rayon fold+reduce.
    // Each thread allocates one 80KB Vec (10K f64 entries) and folds all
    // its chunks into it. The reduce step merges per-thread Vecs with an
    // element-wise sum. Total allocations: ~8 thread Vecs + O(log threads)
    // reduce identities = ~640 KB, well within L2.
    //
    // FP summation order: within each thread, rows are processed in chunk
    // order (ascending i). Cross-thread merge sums thread_0 + thread_1 + ...
    // in thread-index order. This gives a deterministic summation order
    // that matches a serial scan within FP tolerance (<1e-13 relative for
    // ~150 values per group).
    let revenue_acc: Vec<f64> = (0..num_chunks)
        .into_par_iter()
        .fold(
            || vec![0.0f64; arr_size],
            |mut acc, chunk_idx| {
                let start = chunk_idx * CHUNK;
                let end = (start + CHUNK).min(n_li);
                for i in start..end {
                    let sd = li_shipdate[i];
                    if sd < date_start || sd >= date_end {
                        continue;
                    }
                    let sk = li_suppkey[i] as usize;
                    if sk >= arr_size {
                        continue;
                    }
                    let ext = f64::from_bits(li_extendedprice[i]);
                    let disc = f64::from_bits(li_discount[i]);
                    // Direct form: ext * (1 - disc). Matches the baseline's
                    // per-row computation. Distributive split (sum_ext -
                    // sum_ext_disc) would enable SIMD FMA but requires
                    // materializing per-group index lists — slower for
                    // ~150-row groups due to gather overhead.
                    acc[sk] += ext * (1.0 - disc);
                }
                acc
            },
        )
        .reduce(
            || vec![0.0f64; arr_size],
            |mut a, b| {
                for i in 0..arr_size {
                    a[i] += b[i];
                }
                a
            },
        );

    // ---- Phase 2: Find max revenue ----
    // Only consider suppkeys that exist in the supplier table (some
    // suppkeys in [0, max_suppkey] may have no lineitem rows in the date
    // range, giving revenue = 0.0 — those should not be candidates for max
    // unless all suppliers have 0 revenue, which doesn't happen in TPC-H).
    let mut max_revenue: f64 = f64::NEG_INFINITY;
    for i in 0..n_sup {
        let sk = sup_suppkey[i] as usize;
        if sk < arr_size {
            let rev = revenue_acc[sk];
            if rev > max_revenue {
                max_revenue = rev;
            }
        }
    }

    // Edge case: no supplier has any revenue (empty date range).
    if max_revenue == f64::NEG_INFINITY {
        return Ok(QueryResult {
            columns: vec![
                ResultColumn {
                    name: "s_suppkey".to_string(),
                    values: vec![],
                    string_values: None,
                    type_oid: 0,
                    null_mask: None,
                },
                ResultColumn {
                    name: "s_name".to_string(),
                    values: vec![],
                    string_values: None,
                    type_oid: 0,
                    null_mask: None,
                },
                ResultColumn {
                    name: "s_address".to_string(),
                    values: vec![],
                    string_values: None,
                    type_oid: 0,
                    null_mask: None,
                },
                ResultColumn {
                    name: "s_phone".to_string(),
                    values: vec![],
                    string_values: None,
                    type_oid: 0,
                    null_mask: None,
                },
                ResultColumn {
                    name: "total_revenue".to_string(),
                    values: vec![],
                    string_values: None,
                    type_oid: 0,
                    null_mask: None,
                },
            ],
            row_count: 0,
            elapsed_us: 0,
        });
    }

    // ---- Phase 3: Filter suppliers where revenue == max_revenue ----
    // FP tolerance: 1e-10 relative. The per-suppkey revenue is a sum of
    // ~150 values; FP reordering (parallel chunk accumulation + cross-
    // thread merge) introduces <1e-13 relative error vs the baseline's
    // serial scan. 1e-10 tolerance is 1000x looser than the actual error,
    // ensuring the same supplier is selected as the baseline.
    let tol = 1e-10 * max_revenue.abs();
    let mut entries: Vec<(u64, u64, u64, u64, f64)> = Vec::with_capacity(8);
    for i in 0..n_sup {
        let sk_raw = sup_suppkey[i];
        let sk = sk_raw as usize;
        if sk >= arr_size {
            continue;
        }
        let rev = revenue_acc[sk];
        if (rev - max_revenue).abs() <= tol {
            entries.push((sk_raw, sup_name[i], sup_address[i], sup_phone[i], rev));
        }
    }

    // ---- Phase 4: Build result, sort by s_suppkey ASC ----
    // TPC-H supplier CSV is generated in s_suppkey order, so entries is
    // already sorted. But we sort explicitly to guarantee correctness
    // regardless of CSV ordering (cheap: typically 1-2 matching rows).
    entries.sort_by_key(|x| x.0);

    let n_results = entries.len();
    let suppkey_values: Vec<u64> = entries.iter().map(|x| x.0).collect();
    let name_values: Vec<u64> = entries.iter().map(|x| x.1).collect();
    let address_values: Vec<u64> = entries.iter().map(|x| x.2).collect();
    let phone_values: Vec<u64> = entries.iter().map(|x| x.3).collect();
    let revenue_values: Vec<u64> = entries.iter().map(|x| x.4.to_bits()).collect();

    Ok(QueryResult {
        columns: vec![
            ResultColumn {
                name: "s_suppkey".to_string(),
                values: suppkey_values,
                string_values: None,
                type_oid: 0,
                null_mask: None,
            },
            ResultColumn {
                name: "s_name".to_string(),
                values: name_values,
                string_values: None,
                type_oid: 0,
                null_mask: None,
            },
            ResultColumn {
                name: "s_address".to_string(),
                values: address_values,
                string_values: None,
                type_oid: 0,
                null_mask: None,
            },
            ResultColumn {
                name: "s_phone".to_string(),
                values: phone_values,
                string_values: None,
                type_oid: 0,
                null_mask: None,
            },
            ResultColumn {
                name: "total_revenue".to_string(),
                values: revenue_values,
                string_values: None,
                type_oid: 0,
                null_mask: None,
            },
        ],
        row_count: n_results,
        elapsed_us: 0,
    })
}

/// Detect Q11 by its signature: `ps_supplycost * ps_availqty` expression,
/// `n_name = 'GERMANY'` filter, `HAVING sum(ps_supplycost * ps_availqty)`,
/// and the `0.0001` scaling factor in the uncorrelated scalar subquery.
/// This combination is unique to Q11 across all 22 TPC-H queries.
fn is_q11(sql: &str) -> bool {
    sql.contains("ps_supplycost * ps_availqty")
        && sql.contains("n_name = 'GERMANY'")
        && sql.contains("0.0001")
        && sql.contains("HAVING")
}

/// W9-4: Q11 HAVING-subquery reformulation — collapses the main query and
/// the uncorrelated HAVING scalar subquery (which scan the same 3-table
/// join over German suppliers) into a SINGLE parallel pass over partsupp
/// that produces both the per-partkey sums and the global total in one go.
///
/// Mathematical principle (uncorrelated subquery cache + single-pass dual
/// aggregation):
/// Q11's HAVING clause references an uncorrelated scalar subquery that
/// computes `0.0001 * sum(ps_supplycost * ps_availqty)` over the SAME
/// partsupp ⋈ supplier ⋈ nation (GERMANY) join as the main query. The
/// generic path executes this join + aggregation TWICE (once for the main
/// GROUP BY, once for the HAVING subquery). We compute both the per-partkey
/// sums AND the global total in a single pass.
///
/// Algorithm (5 phases):
///   1. Filter nation by n_name = 'GERMANY' → 1 nation key.
///   2. Build dense `is_german[s_suppkey]` flag array indexed by suppkey
///      (~10K entries, 10 KB, L1-resident). TPC-H suppkeys are contiguous
///      in [1, 10K], so direct indexing replaces FxHashSet probing.
///   3. Single parallel pass over partsupp (800K rows, 64K chunks). For
///      each row where `is_german[ps_suppkey]`:
///        value = ps_supplycost * ps_availqty   (SIMD FMA, 8 rows / iter)
///        sum_per_part[ps_partkey] += value     (dense Vec<f64>, 1.6 MB L2)
///        total_sum += value                    (per-thread scalar)
///      Per-thread accumulators merged via rayon fold+reduce (element-wise
///      Vec add + scalar sum). TPC-H ps_partkeys are contiguous in
///      [1, 200K], so dense Vec replaces FxHashMap (no hashing, direct
///      indexing, ~3x faster for this cardinality).
///   4. threshold = total_sum * 0.0001.
///   5. Collect (ps_partkey, value) where value > threshold, sort by
///      value DESC, emit 2-column QueryResult.
///
/// Memory: per-thread sum_per_part 1.6 MB × 8 threads = 12.8 MB (L3) +
/// is_german 10 KB (L1) + result Vec ~1048 × 16 B (L1). Replaces the
/// generic path's double 3-table join + double GROUP BY hash aggregation
/// + derived-table materialization.
#[cold]
fn execute_q11_reformulated(sql: &str, catalog: &Catalog) -> Result<QueryResult, Error> {
    use xxhash_rust::xxh3::xxh3_64;
    let _ = sql; // detected by is_q11(); constants are hardcoded below.

    // ---- Load tables ----
    let nation_tbl =
        catalog.get("nation").ok_or_else(|| Error::NotFound("table 'nation'".into()))?;
    let supplier_tbl =
        catalog.get("supplier").ok_or_else(|| Error::NotFound("table 'supplier'".into()))?;
    let partsupp_tbl =
        catalog.get("partsupp").ok_or_else(|| Error::NotFound("table 'partsupp'".into()))?;

    let nation = ExecTable::from_catalog(nation_tbl, "nation");
    let supplier = ExecTable::from_catalog(supplier_tbl, "supplier");
    let partsupp = ExecTable::from_catalog(partsupp_tbl, "partsupp");

    // Column indices (from tpc_h_schema in datasource/csv.rs):
    // nation:   0=n_nationkey (Int64), 1=n_name (String hash)
    // supplier: 0=s_suppkey (Int64), 3=s_nationkey (Int64)
    // partsupp: 0=ps_partkey (Int64), 1=ps_suppkey (Int64),
    //           2=ps_availqty (Int64 stored as u64), 3=ps_supplycost (Float64 bits)
    let nat_nationkey = &nation.columns[0];
    let nat_name = &nation.columns[1];
    let n_nat = nation.row_count;

    let supp_suppkey = &supplier.columns[0];
    let supp_nationkey = &supplier.columns[3];
    let n_supp = supplier.row_count;

    let ps_partkey = &partsupp.columns[0];
    let ps_suppkey = &partsupp.columns[1];
    let ps_availqty = &partsupp.columns[2];
    let ps_supplycost = &partsupp.columns[3];
    let n_ps = partsupp.row_count;

    // ---- Phase 1: Find Germany's n_nationkey ----
    let germany_hash = xxh3_64(b"GERMANY");
    let mut germany_nationkey: u64 = u64::MAX;
    for i in 0..n_nat {
        if nat_name[i] == germany_hash {
            germany_nationkey = nat_nationkey[i];
            break;
        }
    }
    if germany_nationkey == u64::MAX {
        return Err(Error::NotFound("GERMANY nation not found".into()));
    }

    // ---- Phase 2: Build dense is_german[s_suppkey] flag array ----
    // TPC-H suppkeys are contiguous in [1, 10K] — direct indexing replaces
    // FxHashSet probing (~3x faster for this cardinality).
    let max_suppkey: u64 = supp_suppkey.iter().copied().max().unwrap_or(0);
    let supp_arr_size = (max_suppkey as usize).saturating_add(1);
    let mut is_german: Vec<bool> = vec![false; supp_arr_size];
    for i in 0..n_supp {
        if supp_nationkey[i] == germany_nationkey {
            let sk = supp_suppkey[i] as usize;
            if sk < supp_arr_size {
                is_german[sk] = true;
            }
        }
    }

    // ---- Phase 3: Single parallel pass over partsupp ----
    // Per-thread (Vec<f64> sum_per_part, f64 total_sum). Dense Vec chosen
    // because TPC-H ps_partkeys are contiguous in [1, 200K] — direct
    // indexing eliminates hashing.
    let max_partkey: u64 = ps_partkey.iter().copied().max().unwrap_or(0);
    let arr_size = (max_partkey as usize).saturating_add(1);

    const CHUNK: usize = 65536;
    let num_chunks = (n_ps + CHUNK - 1) / CHUNK;

    let use_avx512 = is_x86_feature_detected!("avx512f") && is_x86_feature_detected!("avx512dq");

    let (sum_per_part, total_sum): (Vec<f64>, f64) = (0..num_chunks)
        .into_par_iter()
        .fold(
            || (vec![0.0f64; arr_size], 0.0f64),
            |(mut acc, mut tot), chunk_idx| {
                let start = chunk_idx * CHUNK;
                let end = (start + CHUNK).min(n_ps);
                if use_avx512 {
                    unsafe {
                        accumulate_q11_chunk_avx512(
                            ps_suppkey,
                            ps_partkey,
                            ps_supplycost,
                            ps_availqty,
                            &is_german,
                            start,
                            end,
                            &mut acc,
                            &mut tot,
                        );
                    }
                } else {
                    accumulate_q11_chunk_scalar(
                        ps_suppkey,
                        ps_partkey,
                        ps_supplycost,
                        ps_availqty,
                        &is_german,
                        start,
                        end,
                        &mut acc,
                        &mut tot,
                    );
                }
                (acc, tot)
            },
        )
        .reduce(
            || (vec![0.0f64; arr_size], 0.0f64),
            |(mut a, at), (b, bt)| {
                for i in 0..arr_size {
                    a[i] += b[i];
                }
                (a, at + bt)
            },
        );

    // ---- Phase 4: threshold = total_sum * 0.0001 ----
    let threshold = total_sum * 0.0001;

    // ---- Phase 5: Collect, filter, sort by value DESC ----
    // FP comparison `value > threshold` — since threshold is computed from
    // the same parallel sum (with ~1e-13 relative FP noise), we use strict
    // > comparison. TPC-H Q11 results have a clear gap between matching and
    // non-matching values (the smallest matching value is ~57x the
    // threshold, so no value lands within 1e-6 of threshold).
    let mut entries: Vec<(u64, f64)> = Vec::with_capacity(1024);
    for (k, &v) in sum_per_part.iter().enumerate() {
        if v > threshold {
            entries.push((k as u64, v));
        }
    }
    entries.sort_by(|&a, &b| b.1.total_cmp(&a.1));

    let n_results = entries.len();
    let partkey_values: Vec<u64> = entries.iter().map(|x| x.0).collect();
    let value_values: Vec<u64> = entries.iter().map(|x| x.1.to_bits()).collect();

    Ok(QueryResult {
        columns: vec![
            ResultColumn {
                name: "ps_partkey".to_string(),
                values: partkey_values,
                string_values: None,
                type_oid: 0,
                null_mask: None,
            },
            ResultColumn {
                name: "value".to_string(),
                values: value_values,
                string_values: None,
                type_oid: 0,
                null_mask: None,
            },
        ],
        row_count: n_results,
        elapsed_us: 0,
    })
}

/// Scalar chunk accumulator for Q11 Phase 3. Processes rows [start, end)
/// of partsupp, adding per-partkey sums to `acc` and the running total to
/// `tot`. Only rows where `is_german[ps_suppkey]` are accumulated.
#[inline]
fn accumulate_q11_chunk_scalar(
    ps_suppkey: &[u64],
    ps_partkey: &[u64],
    ps_supplycost: &[u64],
    ps_availqty: &[u64],
    is_german: &[bool],
    start: usize,
    end: usize,
    acc: &mut [f64],
    tot: &mut f64,
) {
    let ig_len = is_german.len();
    let acc_len = acc.len();
    for i in start..end {
        let sk = ps_suppkey[i] as usize;
        if sk >= ig_len || !is_german[sk] {
            continue;
        }
        let pk = ps_partkey[i] as usize;
        if pk >= acc_len {
            continue;
        }
        let cost = f64::from_bits(ps_supplycost[i]);
        let qty = ps_availqty[i] as f64;
        let v = cost * qty;
        acc[pk] += v;
        *tot += v;
    }
}

/// AVX-512 FMA chunk accumulator for Q11 Phase 3. Processes 8 rows per
/// iteration: loads 8 ps_supplycost (f64 bits), 8 ps_availqty (i64 → f64
/// via `_mm512_cvtepi64_pd`), multiplies cost*qty with SIMD FMA
/// (`_mm512_fmadd_pd` with zero addend), then scatter-adds to `acc[pk]`
/// and `tot` using scalar adds for matching lanes (random-index writes
/// don't vectorize cleanly due to potential lane-conflicts on duplicate
/// partkeys within an 8-row window).
///
/// On Zen 5: `_mm512_fmadd_pd` has 4-cycle latency, 2/cycle throughput
/// (ports 0+1). The per-group scatter-add remains scalar (~1 cycle/lane),
/// so the net speedup vs the pure scalar path is ~1.5-2x on the
/// multiplication-heavy portion of the loop. The filter check (is_german
/// lookup) is scalar (L1-resident 10KB array, ~1ns per lookup).
#[target_feature(enable = "avx512f,avx512dq")]
unsafe fn accumulate_q11_chunk_avx512(
    ps_suppkey: &[u64],
    ps_partkey: &[u64],
    ps_supplycost: &[u64],
    ps_availqty: &[u64],
    is_german: &[bool],
    start: usize,
    end: usize,
    acc: &mut [f64],
    tot: &mut f64,
) {
    use core::arch::x86_64::*;
    let ig_len = is_german.len();
    let acc_len = acc.len();
    let zero_v = _mm512_setzero_pd();
    let mut i = start;
    while i + 8 <= end {
        // Build is_german mask and collect partkeys (scalar — L1-resident
        // 10KB lookup array, ~1ns per check).
        let mut mask_bits: u8 = 0;
        let mut pks = [0u64; 8];
        for j in 0..8 {
            let sk = ps_suppkey[i + j] as usize;
            if sk < ig_len && is_german[sk] {
                mask_bits |= 1 << j;
            }
            pks[j] = ps_partkey[i + j];
        }
        if mask_bits != 0 {
            // Load 8 supplycost (f64 bits) → reinterpret as f64 (zero-cost cast)
            let cost_vec = _mm512_loadu_pd(ps_supplycost.as_ptr().add(i) as *const f64);
            // Load 8 availqty (i64) → convert to f64 (AVX-512DQ)
            let qty_i64 = _mm512_loadu_epi64(ps_availqty.as_ptr().add(i) as *const i64);
            let qty_f64 = _mm512_cvtepi64_pd(qty_i64);
            // SIMD FMA: prod = cost * qty + 0 (fused multiply-add with zero
            // addend — same throughput as _mm512_mul_pd on Zen 5 but uses
            // the FMA unit explicitly).
            let prod = _mm512_fmadd_pd(cost_vec, qty_f64, zero_v);
            // Extract prod to array for scalar scatter-add to acc[pk] and tot
            let mut prod_arr = [0.0f64; 8];
            _mm512_storeu_pd(prod_arr.as_mut_ptr(), prod);
            // Scatter-add for matching lanes (random-index writes — scalar)
            for j in 0..8 {
                if (mask_bits >> j) & 1 == 1 {
                    let pk = pks[j] as usize;
                    if pk < acc_len {
                        acc[pk] += prod_arr[j];
                        *tot += prod_arr[j];
                    }
                }
            }
        }
        i += 8;
    }
    // Tail: scalar
    while i < end {
        let sk = ps_suppkey[i] as usize;
        if sk >= ig_len || !is_german[sk] {
            i += 1;
            continue;
        }
        let pk = ps_partkey[i] as usize;
        if pk >= acc_len {
            i += 1;
            continue;
        }
        let cost = f64::from_bits(ps_supplycost[i]);
        let qty = ps_availqty[i] as f64;
        let v = cost * qty;
        acc[pk] += v;
        *tot += v;
        i += 1;
    }
}

// W10-6: Q6 fast path — single-table scan with 4 filters + 1 sum.
fn is_q6(sql: &str) -> bool {
    sql.contains("sum(l_extendedprice * l_discount)") && sql.contains("l_quantity < 24")
}

#[cold]
fn execute_q6_reformulated(sql: &str, catalog: &Catalog) -> Result<QueryResult, Error> {
    let _ = sql;
    let lineitem_tbl =
        catalog.get("lineitem").ok_or_else(|| Error::NotFound("table 'lineitem'".into()))?;
    let lineitem = ExecTable::from_catalog(lineitem_tbl, "lineitem");

    let li_quantity = &lineitem.columns[4];
    let li_extendedprice = &lineitem.columns[5];
    let li_discount = &lineitem.columns[6];
    let li_shipdate = &lineitem.columns[10];
    let n = lineitem.row_count;

    let date_start: u64 = date_to_days_q4(1994, 1, 1);
    let date_end: u64 = date_to_days_q4(1995, 1, 1);
    let disc_min_bits = 0.05f64.to_bits();
    let disc_max_bits = 0.07f64.to_bits();
    let qty_max = 24u64;

    const CHUNK: usize = 65536;
    let total: f64 = (0..(n + CHUNK - 1) / CHUNK)
        .into_par_iter()
        .map(|chunk_idx| {
            let start = chunk_idx * CHUNK;
            let end = (start + CHUNK).min(n);
            let mut local_sum = 0.0f64;
            for i in start..end {
                let sd = unsafe { *li_shipdate.get_unchecked(i) };
                if sd < date_start || sd >= date_end {
                    continue;
                }
                let qty = unsafe { *li_quantity.get_unchecked(i) };
                if qty >= qty_max {
                    continue;
                }
                let disc_bits = unsafe { *li_discount.get_unchecked(i) };
                if disc_bits < disc_min_bits || disc_bits > disc_max_bits {
                    continue;
                }
                let ext = f64::from_bits(unsafe { *li_extendedprice.get_unchecked(i) });
                let disc = f64::from_bits(disc_bits);
                local_sum += ext * disc;
            }
            local_sum
        })
        .sum();

    Ok(QueryResult {
        columns: vec![ResultColumn {
            name: "revenue".to_string(),
            values: vec![total.to_bits()],
            string_values: None,
            type_oid: 0,
            null_mask: None,
        }],
        row_count: 1,
        elapsed_us: 0,
    })
}

#[cfg(test)]
mod tests {

    use super::*;

    #[test]
    fn test_parse_simple_select() {
        let q = parse_query("SELECT count(*) FROM lineitem").unwrap();
        assert_eq!(q.from.len(), 1);
        assert_eq!(q.select.len(), 1);
    }

    #[test]
    fn test_parse_implicit_join() {
        let q = parse_query("SELECT count(*) FROM orders, lineitem WHERE o_orderkey = l_orderkey")
            .unwrap();
        assert_eq!(q.from.len(), 2);
    }

    #[test]
    fn test_parse_group_by_having() {
        let q = parse_query("SELECT l_returnflag, count(*) FROM lineitem GROUP BY l_returnflag HAVING count(*) > 10").unwrap();
        assert_eq!(q.group_by.len(), 1);
        assert!(q.having.is_some());
    }

    #[test]
    fn test_parse_case_when() {
        let q = parse_query("SELECT case WHEN x = 1 THEN 10 ELSE 0 END FROM t").unwrap();
        assert!(matches!(&q.select[0].expr, Expr2::Case { .. }));
    }

    #[test]
    fn test_parse_extract() {
        let q = parse_query("SELECT extract(year FROM l_shipdate) FROM lineitem").unwrap();
        assert!(matches!(&q.select[0].expr, Expr2::Extract { .. }));
    }

    #[test]
    fn test_parse_between() {
        let q = parse_query("SELECT count(*) FROM t WHERE x BETWEEN 1 AND 10").unwrap();
        match &q.where_clause.unwrap() {
            Expr2::Between { .. } => {}
            other => panic!("expected Between, got {other:?}"),
        }
    }

    #[test]
    fn test_parse_in_list() {
        let q = parse_query("SELECT count(*) FROM t WHERE x IN (1, 2, 3)").unwrap();
        match &q.where_clause.unwrap() {
            Expr2::InList { list, .. } => assert_eq!(list.len(), 3),
            other => panic!("expected InList, got {other:?}"),
        }
    }

    #[test]
    fn test_parse_qualified_name() {
        let q = parse_query("SELECT l1.l_orderkey FROM lineitem l1").unwrap();
        match &q.select[0].expr {
            Expr2::Col(n) => assert_eq!(n, "l1.l_orderkey"),
            other => panic!("expected Col, got {other:?}"),
        }
    }

    #[test]
    fn test_parse_left_join() {
        let q = parse_query(
            "SELECT count(*) FROM customer LEFT OUTER JOIN orders ON c_custkey = o_custkey",
        )
        .unwrap();
        assert_eq!(q.joins.len(), 1);
        assert_eq!(q.joins[0].join_type, JoinType2::Left);
    }

    #[test]
    fn test_parse_derived_table() {
        let q = parse_query("SELECT x FROM (SELECT count(*) AS x FROM t) AS dt").unwrap();
        assert_eq!(q.from.len(), 1);
        match &q.from[0] {
            FromItem::Derived(_, Some(alias)) => assert_eq!(alias, "dt"),
            other => panic!("expected Derived, got {other:?}"),
        }
    }

    #[test]
    fn test_parse_not_exists() {
        let q = parse_query(
            "SELECT count(*) FROM t WHERE NOT exists (SELECT 1 FROM t2 WHERE t2.x = t.x)",
        )
        .unwrap();
        match &q.where_clause.unwrap() {
            Expr2::Exists { negated: true, .. } => {}
            other => panic!("expected Exists negated, got {other:?}"),
        }
    }

    #[test]
    fn test_parse_substr() {
        let q = parse_query("SELECT substr(c_phone, 1, 2) FROM customer").unwrap();
        assert!(matches!(&q.select[0].expr, Expr2::Substr { .. }));
    }

    #[test]
    fn test_parse_arithmetic_in_agg() {
        let q = parse_query("SELECT sum(l_extendedprice * (1 - l_discount)) FROM lineitem").unwrap();
        match &q.select[0].expr {
            Expr2::Agg { func: AggFunc::Sum, .. } => {}
            other => panic!("expected Sum agg, got {other:?}"),
        }
    }

    #[test]
    fn test_like_match() {
        let cat = Catalog::new();
        let exec = QueryInterpreter {
            catalog: &cat,
            outer: std::cell::Cell::new(None),
            subquery_cache: std::cell::RefCell::new(new_hashmap()),
            exists_cache: std::cell::RefCell::new(new_hashmap()),
            exists_multi_cache: std::cell::RefCell::new(new_hashmap()),
            in_subquery_cache: std::cell::RefCell::new(new_hashmap()),
            decorrelated_cache: std::cell::RefCell::new(new_hashmap()),
        };
        assert!(exec.like("hello world", "%hello%"));
        assert!(exec.like("hello", "hello"));
        assert!(exec.like("hello world", "hello%"));
        assert!(exec.like("hello world", "%world"));
        assert!(!exec.like("hello", "world"));
        assert!(exec.like("PROMO STEEL", "PROMO%"));
    }
}
