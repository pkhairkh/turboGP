//! Query interpreter parser — parses SQL into SelectQuery2.

use crate::catalog::Catalog;
use crate::datasource::table::Table;
use crate::engine::result::QueryResult;
use crate::sql::lexer::{tokenize, Token};
use crate::Error;
use fxhash::{FxHashMap, FxHashSet};

use super::types::*;
use super::{HashMap, HashSet, new_hashmap, new_hashset, new_fxhashmap, new_fxhashset};

pub fn parse_query(sql: &str) -> Result<SelectQuery2, String> {
    let tokens = tokenize(sql)?;
    let mut p = QueryInterpreterParser { tokens, pos: 0 };
    let q = p.parse_select()?;
    match p.peek() {
        Token::Semicolon | Token::EOF => Ok(q),
        other => Err(format!("unexpected trailing token: {other:?}")),
    }
}

pub(crate) struct QueryInterpreterParser {
    pub(crate) tokens: Vec<Token>,
    pub(crate) pos: usize,
}


impl QueryInterpreterParser {
    pub(crate) fn peek(&self) -> &Token {
        &self.tokens[self.pos]
    }
    pub(crate) fn peek_at(&self, n: usize) -> &Token {
        self.tokens.get(self.pos + n).unwrap_or(&Token::EOF)
    }
    pub(crate) fn next(&mut self) -> Token {
        let t = self.tokens.get(self.pos).cloned().unwrap_or(Token::EOF);
        if !matches!(t, Token::EOF) {
            self.pos += 1;
        }
        t
    }
    pub(crate) fn match_kw(&mut self, kw: &str) -> bool {
        if let Token::Keyword(k) = self.peek() {
            if k == kw {
                self.pos += 1;
                return true;
            }
        }
        false
    }
    pub(crate) fn match_ident_or_kw(&mut self, name: &str) -> bool {
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
    pub(crate) fn expect_kw(&mut self, kw: &str) -> Result<(), String> {
        if self.match_kw(kw) {
            return Ok(());
        }
        Err(format!("expected keyword {kw}, got {:?}", self.peek()))
    }
    pub(crate) fn match_op(&mut self, op: &str) -> bool {
        if let Token::Op(o) = self.peek() {
            if o == op {
                self.pos += 1;
                return true;
            }
        }
        false
    }
    pub(crate) fn is_op(&self, ops: &[&str]) -> bool {
        if let Token::Op(o) = self.peek() {
            ops.contains(&o.as_str())
        } else {
            false
        }
    }
    pub(crate) fn match_lp(&mut self) -> bool {
        if matches!(self.peek(), Token::LParen) {
            self.pos += 1;
            true
        } else {
            false
        }
    }
    pub(crate) fn expect_lp(&mut self) -> Result<(), String> {
        if self.match_lp() {
            Ok(())
        } else {
            Err(format!("expected '(', got {:?}", self.peek()))
        }
    }
    pub(crate) fn expect_rp(&mut self) -> Result<(), String> {
        if matches!(self.peek(), Token::RParen) {
            self.pos += 1;
            Ok(())
        } else {
            Err(format!("expected ')', got {:?}", self.peek()))
        }
    }
    pub(crate) fn match_comma(&mut self) -> bool {
        if matches!(self.peek(), Token::Comma) {
            self.pos += 1;
            true
        } else {
            false
        }
    }
    pub(crate) fn is_clause_boundary(&self) -> bool {
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

    pub(crate) fn parse_ident_name(&mut self) -> Result<String, String> {
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

    pub(crate) fn parse_select(&mut self) -> Result<SelectQuery2, String> {
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

    pub(crate) fn parse_select_list(&mut self) -> Result<Vec<SelectItem2>, String> {
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

    pub(crate) fn parse_from_list(&mut self) -> Result<Vec<FromItem>, String> {
        let mut items = Vec::new();
        loop {
            items.push(self.parse_from_item()?);
            if !self.match_comma() {
                break;
            }
        }
        Ok(items)
    }

    pub(crate) fn parse_from_item(&mut self) -> Result<FromItem, String> {
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

    pub(crate) fn parse_expr_list(&mut self) -> Result<Vec<Expr2>, String> {
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

    pub(crate) fn parse_order_list(&mut self) -> Result<Vec<(Expr2, bool)>, String> {
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

    pub(crate) fn parse_usize(&mut self) -> Result<usize, String> {
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

    pub(crate) fn parse_expr(&mut self) -> Result<Expr2, String> {
        self.parse_or()
    }

    pub(crate) fn parse_or(&mut self) -> Result<Expr2, String> {
        let mut left = self.parse_and()?;
        while self.match_kw("OR") {
            let right = self.parse_and()?;
            left = Expr2::BinOp { op: BinOp2::Or, left: Box::new(left), right: Box::new(right) };
        }
        Ok(left)
    }

    pub(crate) fn parse_and(&mut self) -> Result<Expr2, String> {
        let mut left = self.parse_not()?;
        while self.match_kw("AND") {
            let right = self.parse_not()?;
            left = Expr2::BinOp { op: BinOp2::And, left: Box::new(left), right: Box::new(right) };
        }
        Ok(left)
    }

    pub(crate) fn parse_not(&mut self) -> Result<Expr2, String> {
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

    pub(crate) fn parse_comparison(&mut self) -> Result<Expr2, String> {
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

    pub(crate) fn parse_in_rest(&mut self, left: Expr2, negated: bool) -> Result<Expr2, String> {
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

    pub(crate) fn parse_between_rest(&mut self, left: Expr2, negated: bool) -> Result<Expr2, String> {
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

    pub(crate) fn parse_additive(&mut self) -> Result<Expr2, String> {
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

    pub(crate) fn parse_multiplicative(&mut self) -> Result<Expr2, String> {
        let mut left = self.parse_unary()?;
        loop {
            if self.is_op(&["*", "/", "%"]) {
                let op_str =
                    if let Token::Op(o) = self.peek() { o.clone() } else { unreachable!() };
                self.next();
                let right = self.parse_unary()?;
                let op = match op_str.as_str() {
                    "*" => BinOp2::Mul,
                    "/" => BinOp2::Div,
                    "%" => BinOp2::Mod,
                    _ => unreachable!(),
                };
                left = Expr2::BinOp { op, left: Box::new(left), right: Box::new(right) };
            } else {
                break;
            }
        }
        Ok(left)
    }

    pub(crate) fn parse_unary(&mut self) -> Result<Expr2, String> {
        if self.match_op("-") {
            return Ok(Expr2::Neg(Box::new(self.parse_unary()?)));
        }
        if self.match_op("+") {
            return self.parse_unary();
        }
        self.parse_primary()
    }

    pub(crate) fn parse_primary(&mut self) -> Result<Expr2, String> {
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

    pub(crate) fn parse_case(&mut self) -> Result<Expr2, String> {
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

    pub(crate) fn parse_extract(&mut self) -> Result<Expr2, String> {
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
    pub(crate) fn parse_cast(&mut self) -> Result<Expr2, String> {
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

    pub(crate) fn parse_substr(&mut self) -> Result<Expr2, String> {
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

    pub(crate) fn parse_agg_call(&mut self, func_upper: &str) -> Result<Expr2, String> {
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


