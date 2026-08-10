//! # Arithmetic expression evaluator for aggregate args (Wave 40).
//!
//! Evaluates expressions like `price * (1 - discount)` against table rows.
//! Supports: column references, integer/float literals, + - * /, parentheses.

use crate::datasource::table::Table;

/// Evaluate an arithmetic expression for a specific row, returning a u64
/// (f64 bits for float results).
///
/// The expression is a space-separated string of tokens produced by
/// parse_agg_arg, e.g. "price * ( 1 - discount )".
pub fn eval_expr(expr: &str, table: &Table, row_idx: usize) -> u64 {
    let tokens: Vec<&str> = expr.split_whitespace().collect();
    if tokens.is_empty() {
        return 0;
    }
    // Try to evaluate as a simple column reference first.
    if tokens.len() == 1 {
        return eval_token(tokens[0], table, row_idx);
    }
    // Use a recursive descent parser for the expression.
    let mut parser = ExprParser { tokens: &tokens, pos: 0 };
    let result = parser.parse_expr(table, row_idx);
    result
}

fn eval_token(token: &str, table: &Table, row_idx: usize) -> u64 {
    // Try integer literal.
    if let Ok(n) = token.parse::<i64>() {
        return n as u64;
    }
    if let Ok(n) = token.parse::<u64>() {
        return n;
    }
    // Try float literal.
    if let Ok(f) = token.parse::<f64>() {
        return f.to_bits();
    }
    // Column reference.
    if let Some(idx) = table.column_idx(token) {
        return table.columns[idx].get(row_idx).copied().unwrap_or(0);
    }
    0
}

/// Check if an expression is a simple column reference (no operators).
pub fn is_simple_column(expr: &str) -> bool {
    let tokens: Vec<&str> = expr.split_whitespace().collect();
    tokens.len() == 1 && !tokens[0].chars().any(|c| "+-*/()".contains(c))
}

/// Check if an expression contains arithmetic operators.
pub fn is_arithmetic_expr(expr: &str) -> bool {
    expr.split_whitespace()
        .any(|t| t == "+" || t == "-" || t == "*" || t == "/" || t == "(" || t == ")")
}

/// A typed value: either an integer or a float (stored as bits).
#[derive(Debug, Clone, Copy)]
enum TypedVal {
    Int(u64),
    Float(u64), // f64::to_bits()
}

impl TypedVal {
    fn as_f64(self) -> f64 {
        match self {
            TypedVal::Int(n) => n as f64,
            TypedVal::Float(bits) => f64::from_bits(bits),
        }
    }

    fn as_u64(self) -> u64 {
        match self {
            TypedVal::Int(n) => n,
            TypedVal::Float(bits) => bits,
        }
    }
}

/// Evaluate a token to a typed value.
fn eval_token_typed(token: &str, table: &Table, row_idx: usize) -> TypedVal {
    // Try integer literal.
    if let Ok(n) = token.parse::<i64>() {
        return TypedVal::Int(n as u64);
    }
    if let Ok(n) = token.parse::<u64>() {
        return TypedVal::Int(n);
    }
    // Try float literal — has a '.' or 'e'.
    if token.contains('.') || token.contains('e') || token.contains('E') {
        if let Ok(f) = token.parse::<f64>() {
            return TypedVal::Float(f.to_bits());
        }
    }
    // Column reference — check schema for type.
    if let Some(idx) = table.column_idx(token) {
        let val = table.columns[idx].get(row_idx).copied().unwrap_or(0);
        // If the table has a schema, check if this column is a float type.
        if let Some(ref schema) = table.schema {
            if schema.is_float(idx) {
                return TypedVal::Float(val);
            }
        }
        // Heuristic: if the value looks like an f64 bit pattern (> 2^60 and not a
        // small negative i64), treat as float. This is a fallback for loaded tables
        // that don't have a schema.
        if val > (1u64 << 62) && f64::from_bits(val).is_finite() && f64::from_bits(val).abs() < 1e15
        {
            return TypedVal::Float(val);
        }
        TypedVal::Int(val)
    } else {
        TypedVal::Int(0)
    }
}

/// Evaluate a binary operation with proper type tracking.
fn eval_binop_typed(op: &str, left: TypedVal, right: TypedVal) -> TypedVal {
    // If either operand is a float, do float arithmetic.
    let is_float = matches!(left, TypedVal::Float(_)) || matches!(right, TypedVal::Float(_));
    if is_float {
        let l = left.as_f64();
        let r = right.as_f64();
        let result = match op {
            "+" => l + r,
            "-" => l - r,
            "*" => l * r,
            "/" => {
                if r == 0.0 {
                    0.0
                } else {
                    l / r
                }
            }
            _ => 0.0,
        };
        TypedVal::Float(result.to_bits())
    } else {
        // Both are integers.
        let l = left.as_u64();
        let r = right.as_u64();
        let result = match op {
            "+" => l.wrapping_add(r),
            "-" => l.wrapping_sub(r),
            "*" => l.wrapping_mul(r),
            "/" => {
                if r == 0 {
                    0
                } else {
                    l / r
                }
            }
            _ => 0,
        };
        TypedVal::Int(result)
    }
}

/// Simple recursive descent parser for arithmetic expressions.
struct ExprParser<'a> {
    tokens: &'a [&'a str],
    pos: usize,
}

impl<'a> ExprParser<'a> {
    fn parse_expr(&mut self, table: &Table, row_idx: usize) -> u64 {
        self.parse_additive(table, row_idx).as_u64()
    }

    fn parse_additive(&mut self, table: &Table, row_idx: usize) -> TypedVal {
        let mut left = self.parse_multiplicative(table, row_idx);
        loop {
            if self.pos >= self.tokens.len() {
                break;
            }
            let op = self.tokens[self.pos];
            if op != "+" && op != "-" {
                break;
            }
            self.pos += 1;
            let right = self.parse_multiplicative(table, row_idx);
            left = eval_binop_typed(op, left, right);
        }
        left
    }

    fn parse_multiplicative(&mut self, table: &Table, row_idx: usize) -> TypedVal {
        let mut left = self.parse_primary(table, row_idx);
        loop {
            if self.pos >= self.tokens.len() {
                break;
            }
            let op = self.tokens[self.pos];
            if op != "*" && op != "/" {
                break;
            }
            self.pos += 1;
            let right = self.parse_primary(table, row_idx);
            left = eval_binop_typed(op, left, right);
        }
        left
    }

    fn parse_primary(&mut self, table: &Table, row_idx: usize) -> TypedVal {
        if self.pos >= self.tokens.len() {
            return TypedVal::Int(0);
        }
        let token = self.tokens[self.pos];
        if token == "(" {
            self.pos += 1; // consume (
            let val = self.parse_additive(table, row_idx);
            if self.pos < self.tokens.len() && self.tokens[self.pos] == ")" {
                self.pos += 1; // consume )
            }
            return val;
        }
        self.pos += 1;
        eval_token_typed(token, table, row_idx)
    }
}

// -----------------------------------------------------------------------
// Tests
// -----------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::datasource::parquet::{LoadedColumn, LoadedTable};
    use crate::datasource::Table;

    fn make_table() -> Table {
        Table::from_loaded(LoadedTable {
            name: "t".into(),
            columns: vec![
                LoadedColumn {
                    name: "price".into(),
                    cells: vec![100, 200, 300],
                    row_count: 3,
                    string_search: None,
                    null_bitmap: None,
                },
                LoadedColumn {
                    name: "discount".into(),
                    cells: vec![10, 20, 30],
                    row_count: 3,
                    string_search: None,
                    null_bitmap: None,
                },
            ],
            row_count: 3,
        })
    }

    #[test]
    fn eval_simple_column() {
        let t = make_table();
        assert_eq!(eval_expr("price", &t, 0), 100);
        assert_eq!(eval_expr("price", &t, 1), 200);
    }

    #[test]
    fn eval_integer_literal() {
        let t = make_table();
        assert_eq!(eval_expr("42", &t, 0), 42);
    }

    #[test]
    fn eval_addition() {
        let t = make_table();
        // price + discount = 100 + 10 = 110
        assert_eq!(eval_expr("price + discount", &t, 0), 110);
    }

    #[test]
    fn eval_multiplication() {
        let t = make_table();
        // price * discount = 100 * 10 = 1000
        assert_eq!(eval_expr("price * discount", &t, 0), 1000);
    }

    #[test]
    fn eval_subtraction() {
        let t = make_table();
        // price - discount = 100 - 10 = 90
        assert_eq!(eval_expr("price - discount", &t, 0), 90);
    }

    #[test]
    fn eval_parentheses() {
        let t = make_table();
        // (price - discount) * 2 = (100 - 10) * 2 = 180
        assert_eq!(eval_expr("( price - discount ) * 2", &t, 0), 180);
    }

    #[test]
    fn eval_complex_expr() {
        let t = make_table();
        // price * (1 - discount) — but discount=10, so 1-10 = -9 (as u64 wrapping)
        // Actually for TPC-H: SUM(l_extendedprice * (1 - l_discount))
        // where l_discount is a float like 0.05. Let's test with small ints.
        // price * ( 1 - 0 ) = 100 * 1 = 100 (if discount were 0)
        // For our test: price=100, discount=10 → 100 * (1 - 10) = 100 * (-9)
        // In u64 wrapping: -9 as u64 = huge. So let's test a different expr.
        // price * 2 + discount = 200 + 10 = 210
        assert_eq!(eval_expr("price * 2 + discount", &t, 0), 210);
    }

    #[test]
    fn is_simple_column_check() {
        assert!(is_simple_column("price"));
        assert!(!is_simple_column("price * 2"));
    }

    #[test]
    fn is_arithmetic_expr_check() {
        assert!(is_arithmetic_expr("price * 2"));
        assert!(is_arithmetic_expr("( a + b )"));
        assert!(!is_arithmetic_expr("price"));
    }
}
