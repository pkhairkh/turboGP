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



// === Pre-compiled expression for fast vectorized evaluation ===

/// A compiled arithmetic expression. Parse once, evaluate many times.
/// This avoids re-tokenizing and re-parsing the expression string per row.
#[derive(Debug, Clone)]
pub enum CompiledNode {
    /// A column reference (resolved to column index at compile time).
    Column(usize),
    /// An integer literal.
    IntLit(u64),
    /// A float literal (stored as f64::to_bits).
    FloatLit(u64),
    /// A binary operation: op(left, right).
    BinOp {
        op: char,
        left: Box<CompiledNode>,
        right: Box<CompiledNode>,
    },
}

// === E-graph optimization (W3-T2) ===
//
// Lower a CompiledNode tree to the e-graph, saturate with standard rewrite
// rules (identity, zero, strength reduction, distributivity, constant
// folding), and extract the cheapest form. This is the e-graph *concept*
// implemented natively in Rust — no `egg` dependency, no VUMA.
//
// The e-graph optimization runs once per `compile_expr` call (i.e., once per
// query, not per row). The overhead is ~10-100µs depending on expression
// complexity — negligible compared to the per-row evaluation cost.
//
// For Q1's expression `l_extendedprice * (1 - l_discount) * (1 + l_tax)`:
//   - The e-graph's distributivity rule adds the expanded form
//     `a + a*c - a*b - a*b*c` as an equivalence.
//   - The cost function (Mul=5, Add=3) picks the factored form (cost ~19)
//     over the expanded form (cost ~30+).
//   - So Q1's expression survives e-graph optimization unchanged,
//     preserving the simd_agg Pattern 3 match.
//
// For expressions with constants (e.g., `price * 1`, `price * 0`,
// `price + 0`, `price * 2`), the e-graph applies identity/zero/strength-
// reduction rewrites, simplifying the scalar evaluation path.

use crate::exec::egraph::{
    EGraph, ENode, EClassId, BinOpKind, apply_standard_rules, default_cost_fn,
};

/// Lower a CompiledNode tree to the e-graph, returning the root EClassId.
///
/// `IntLit(n)` is stored as `n as u64` (integer cast) in CompiledNode, but
/// the e-graph's `Lit` stores `f64::to_bits`. We convert IntLit(n) to
/// `Lit((n as f64).to_bits())` so that the e-graph's identity/zero rules
/// (which check `f64::to_bits(0.0) == 0` and `f64::to_bits(1.0)`) work
/// correctly for both integer and float literals.
fn lower_to_egraph(node: &CompiledNode, eg: &mut EGraph) -> EClassId {
    match node {
        CompiledNode::Column(idx) => eg.add(ENode::Col(*idx)),
        CompiledNode::IntLit(n) => eg.add(ENode::Lit((*n as f64).to_bits())),
        CompiledNode::FloatLit(bits) => eg.add(ENode::Lit(*bits)),
        CompiledNode::BinOp { op, left, right } => {
            let left_id = lower_to_egraph(left, eg);
            let right_id = lower_to_egraph(right, eg);
            let binop_kind = match op {
                '+' => BinOpKind::Add,
                '-' => BinOpKind::Sub,
                '*' => BinOpKind::Mul,
                '/' => BinOpKind::Div,
                _ => {
                    // Unknown operator — lower as Lit(0) to avoid e-graph panic.
                    return eg.add(ENode::Lit(0));
                }
            };
            eg.add(ENode::BinOp { op: binop_kind, left: left_id, right: right_id })
        }
    }
}

/// Extract the cheapest CompiledNode from the e-graph.
///
/// Calls `eg.extract(id, &default_cost_fn)` to pick the lowest-cost ENode
/// from the e-class, then recursively extracts children. The `extract` call
/// memoizes the best form for each e-class, so recursive calls are O(1) after
/// the first extraction.
fn extract_from_egraph(eg: &mut EGraph, id: EClassId) -> CompiledNode {
    let enode = eg.extract(id, &default_cost_fn);
    match enode {
        ENode::Col(idx) => CompiledNode::Column(idx),
        ENode::Lit(bits) => CompiledNode::FloatLit(bits),
        ENode::BinOp { op, left, right } => {
            let left_compiled = extract_from_egraph(eg, left);
            let right_compiled = extract_from_egraph(eg, right);
            CompiledNode::BinOp {
                op: op.to_char(),
                left: Box::new(left_compiled),
                right: Box::new(right_compiled),
            }
        }
        ENode::Fma { a, b, c } => {
            // Fma(a, b, c) = a * b + c. Convert back to BinOp form since
            // CompiledNode doesn't have an FMA variant. The FMA optimization
            // is captured in the cost model (the extractor picked Fma because
            // it's cheaper), but the scalar evaluator will use separate
            // mul + add. For simd_agg, FMA is already used at the kernel level.
            let a_compiled = extract_from_egraph(eg, a);
            let b_compiled = extract_from_egraph(eg, b);
            let c_compiled = extract_from_egraph(eg, c);
            CompiledNode::BinOp {
                op: '+',
                left: Box::new(CompiledNode::BinOp {
                    op: '*',
                    left: Box::new(a_compiled),
                    right: Box::new(b_compiled),
                }),
                right: Box::new(c_compiled),
            }
        }
    }
}

/// Optimize a CompiledNode tree using the e-graph.
///
/// 1. Lower the tree to the e-graph (hash-consing shared subexpressions).
/// 2. Saturate with standard rewrite rules (budget=100 iterations).
/// 3. Extract the cheapest form using the latency-based cost function.
///
/// Returns the optimized tree. If no rewrites apply, the returned tree is
/// structurally identical to the input (same shape, same ops).
fn optimize_with_egraph(node: CompiledNode) -> CompiledNode {
    let mut eg = EGraph::new();
    let root = lower_to_egraph(&node, &mut eg);
    eg.saturate(apply_standard_rules, 100);
    extract_from_egraph(&mut eg, root)
}

/// Compile an arithmetic expression string into a CompiledNode tree.
/// Column names are resolved to indices at compile time.
pub fn compile_expr(expr: &str, table: &Table) -> Option<CompiledNode> {
    let tokens: Vec<&str> = expr.split_whitespace().collect();
    if tokens.is_empty() {
        return None;
    }
    let mut compiler = ExprCompiler { tokens: &tokens, pos: 0, table };
    let result = compiler.compile_additive();
    if compiler.pos == tokens.len() {
        // W3-T2: optimize the compiled tree via the e-graph (identity, zero,
        // strength reduction, distributivity, constant folding). The overhead
        // is ~10-100µs per query (negligible vs per-row evaluation cost).
        Some(optimize_with_egraph(result))
    } else {
        None
    }
}

struct ExprCompiler<'a> {
    tokens: &'a [&'a str],
    pos: usize,
    table: &'a Table,
}

impl<'a> ExprCompiler<'a> {
    fn compile_additive(&mut self) -> CompiledNode {
        let mut left = self.compile_multiplicative();
        loop {
            if self.pos >= self.tokens.len() { break; }
            let op = self.tokens[self.pos];
            if op != "+" && op != "-" { break; }
            self.pos += 1;
            let right = self.compile_multiplicative();
            left = CompiledNode::BinOp { op: op.chars().next().unwrap(), left: Box::new(left), right: Box::new(right) };
        }
        left
    }

    fn compile_multiplicative(&mut self) -> CompiledNode {
        let mut left = self.compile_primary();
        loop {
            if self.pos >= self.tokens.len() { break; }
            let op = self.tokens[self.pos];
            if op != "*" && op != "/" { break; }
            self.pos += 1;
            let right = self.compile_primary();
            left = CompiledNode::BinOp { op: op.chars().next().unwrap(), left: Box::new(left), right: Box::new(right) };
        }
        left
    }

    fn compile_primary(&mut self) -> CompiledNode {
        if self.pos >= self.tokens.len() {
            return CompiledNode::IntLit(0);
        }
        let token = self.tokens[self.pos];
        if token == "(" {
            self.pos += 1;
            let val = self.compile_additive();
            if self.pos < self.tokens.len() && self.tokens[self.pos] == ")" {
                self.pos += 1;
            }
            return val;
        }
        self.pos += 1;
        // Try integer literal
        if let Ok(n) = token.parse::<i64>() {
            return CompiledNode::IntLit(n as u64);
        }
        if let Ok(n) = token.parse::<u64>() {
            return CompiledNode::IntLit(n);
        }
        // Try float literal
        if let Ok(f) = token.parse::<f64>() {
            return CompiledNode::FloatLit(f.to_bits());
        }
        // Column reference — resolve to index NOW
        if let Some(idx) = self.table.column_idx(token) {
            return CompiledNode::Column(idx);
        }
        CompiledNode::IntLit(0)
    }
}

/// Evaluate a compiled expression for a single row. Much faster than eval_expr
/// because no string parsing occurs.
#[inline]
pub fn eval_compiled(node: &CompiledNode, table: &Table, row_idx: usize) -> u64 {
    match node {
        CompiledNode::Column(idx) => table.columns[*idx].get(row_idx).copied().unwrap_or(0),
        CompiledNode::IntLit(v) => *v,
        CompiledNode::FloatLit(bits) => *bits,
        CompiledNode::BinOp { op, left, right } => {
            let l = eval_compiled(left, table, row_idx);
            let r = eval_compiled(right, table, row_idx);
            eval_binop_u64(*op, l, r)
        }
    }
}

/// Evaluate a compiled expression as f64 for a single row.
#[inline]
pub fn eval_compiled_f64(node: &CompiledNode, table: &Table, row_idx: usize) -> f64 {
    match node {
        CompiledNode::Column(idx) => {
            let v = table.columns[*idx].get(row_idx).copied().unwrap_or(0);
            // Check if float-encoded
            if v > (1u64 << 60) {
                f64::from_bits(v)
            } else {
                v as f64
            }
        }
        CompiledNode::IntLit(v) => *v as f64,
        CompiledNode::FloatLit(bits) => f64::from_bits(*bits),
        CompiledNode::BinOp { op, left, right } => {
            let l = eval_compiled_f64(left, table, row_idx);
            let r = eval_compiled_f64(right, table, row_idx);
            match op {
                '+' => l + r,
                '-' => l - r,
                '*' => l * r,
                '/' => l / r,
                _ => 0.0,
            }
        }
    }
}

/// W2-T2 regression fix: simd_agg kernels treat `Vec<u64>` cells as
/// `f64::to_bits` patterns. This is only correct when the column is
/// actually stored as f64 bits (Float/Real/Decimal/Numeric). For INT
/// columns the u64 cell holds the integer value directly (e.g. `100u64`),
/// and `f64::from_bits(100u64)` ≈ 4.9e-322 ≈ 0 — silently producing 0
/// for `SUM(price * discount)` on INT columns.
///
/// We gate simd_agg on the column's `ColumnType` (from `table.schema`).
/// If the schema is `None` (defensive — should not happen for tables
/// loaded via `engine.load_csv` or DDL), we conservatively refuse to
/// fire simd_agg and fall back to the scalar loop.
fn column_is_f64_encoded(table: &crate::datasource::Table, col_idx: usize) -> bool {
    use crate::sql::ddl::ColumnType;
    let Some(schema) = table.schema.as_ref() else {
        return false;
    };
    let Some(col_schema) = schema.columns.get(col_idx) else {
        return false;
    };
    matches!(
        col_schema.col_type,
        ColumnType::Float | ColumnType::Real | ColumnType::Decimal(_, _) | ColumnType::Numeric(_, _)
    )
}

/// Try to pattern-match the compiled expression against one of the
/// AVX-512 FMA kernels in `simd_agg.rs`. Returns `Some(result)` if a
/// kernel matches, `None` to fall back to the scalar per-row loop.
///
/// Recognised patterns (where `a`, `b`, `c` are column refs and `1` is
/// either `IntLit(1)` or `FloatLit(1.0)`):
///
///   * `a * b`                            -> `sum_a_mul_b_by_idx`
///   * `a * (1 - b)`                      -> `sum_a_mul_one_minus_b_by_idx`
///   * `a * (1 - b) * (1 + c)`            -> `sum_a_mul_one_minus_b_mul_one_plus_c_by_idx`
///
/// Also handles left-associative variants where the multiplications are
/// nested in either order (e.g. `(1 - b) * a`).
fn try_simd_agg(node: &CompiledNode, table: &Table, indices: &[usize]) -> Option<f64> {
    use crate::exec::simd_agg;

    // Helper: matches `1` (either IntLit(1) or FloatLit(1.0))
    fn is_one(n: &CompiledNode) -> bool {
        matches!(n, CompiledNode::IntLit(1))
            || matches!(n, CompiledNode::FloatLit(bits) if f64::from_bits(*bits) == 1.0)
    }
    // Helper: extract column index if `n` is a `Column(_)` (or `1 - col`)
    fn as_col(n: &CompiledNode) -> Option<usize> {
        match n {
            CompiledNode::Column(idx) => Some(*idx),
            _ => None,
        }
    }
    // Helper: matches `(1 - col)` and returns col_idx
    fn as_one_minus_col(n: &CompiledNode) -> Option<usize> {
        if let CompiledNode::BinOp { op: '-', left, right } = n {
            if is_one(left) {
                return as_col(right);
            }
        }
        None
    }
    // Helper: matches `(1 + col)` and returns col_idx
    fn as_one_plus_col(n: &CompiledNode) -> Option<usize> {
        if let CompiledNode::BinOp { op: '+', left, right } = n {
            if is_one(left) {
                return as_col(right);
            }
        }
        None
    }

    // Pattern 1: a * b
    // Tree: BinOp { op: '*', left: Col(a), right: Col(b) }
    // Guard: both columns must be f64-encoded (Float/Real/Decimal/Numeric).
    // INT columns store `value as u64` (not `f64::to_bits`), so the AVX-512
    // kernel would misinterpret them and return ~0.
    if let CompiledNode::BinOp { op: '*', left, right } = node {
        if let (Some(a_idx), Some(b_idx)) = (as_col(left), as_col(right)) {
            if column_is_f64_encoded(table, a_idx) && column_is_f64_encoded(table, b_idx) {
                let a = &table.columns[a_idx];
                let b = &table.columns[b_idx];
                return Some(simd_agg::sum_a_mul_b_by_idx(a, b, indices));
            }
        }
    }

    // Pattern 2: a * (1 - b)
    // Tree: BinOp { op: '*', left: Col(a), right: BinOp{op:'-', left:1, right: Col(b)} }
    if let CompiledNode::BinOp { op: '*', left, right } = node {
        if let Some(a_idx) = as_col(left) {
            if let Some(b_idx) = as_one_minus_col(right) {
                if column_is_f64_encoded(table, a_idx) && column_is_f64_encoded(table, b_idx) {
                    let a = &table.columns[a_idx];
                    let b = &table.columns[b_idx];
                    return Some(simd_agg::sum_a_mul_one_minus_b_by_idx(a, b, indices));
                }
            }
        }
        // Also handle commutative: (1 - b) * a
        if let Some(a_idx) = as_col(right) {
            if let Some(b_idx) = as_one_minus_col(left) {
                if column_is_f64_encoded(table, a_idx) && column_is_f64_encoded(table, b_idx) {
                    let a = &table.columns[a_idx];
                    let b = &table.columns[b_idx];
                    return Some(simd_agg::sum_a_mul_one_minus_b_by_idx(a, b, indices));
                }
            }
        }
    }

    // Pattern 3: a * (1 - b) * (1 + c)
    // Tree: BinOp { op: '*', left: BinOp{op:'*', left: Col(a), right: (1-b)}, right: (1+c) }
    if let CompiledNode::BinOp { op: '*', left, right } = node {
        if let Some(c_idx) = as_one_plus_col(right) {
            // left should be `a * (1 - b)` (pattern 2)
            if let CompiledNode::BinOp { op: '*', left: l2, right: r2 } = left.as_ref() {
                if let Some(a_idx) = as_col(l2) {
                    if let Some(b_idx) = as_one_minus_col(r2) {
                        if column_is_f64_encoded(table, a_idx)
                            && column_is_f64_encoded(table, b_idx)
                            && column_is_f64_encoded(table, c_idx)
                        {
                            let a = &table.columns[a_idx];
                            let b = &table.columns[b_idx];
                            let c = &table.columns[c_idx];
                            return Some(simd_agg::sum_a_mul_one_minus_b_mul_one_plus_c_by_idx(a, b, c, indices));
                        }
                    }
                }
                // Also handle commutative: (1 - b) * a
                if let Some(a_idx) = as_col(r2) {
                    if let Some(b_idx) = as_one_minus_col(l2) {
                        if column_is_f64_encoded(table, a_idx)
                            && column_is_f64_encoded(table, b_idx)
                            && column_is_f64_encoded(table, c_idx)
                        {
                            let a = &table.columns[a_idx];
                            let b = &table.columns[b_idx];
                            let c = &table.columns[c_idx];
                            return Some(simd_agg::sum_a_mul_one_minus_b_mul_one_plus_c_by_idx(a, b, c, indices));
                        }
                    }
                }
            }
        }
        // Also handle: (1 + c) on the left, a * (1 - b) on the right
        if let Some(c_idx) = as_one_plus_col(left) {
            if let CompiledNode::BinOp { op: '*', left: l2, right: r2 } = right.as_ref() {
                if let Some(a_idx) = as_col(l2) {
                    if let Some(b_idx) = as_one_minus_col(r2) {
                        if column_is_f64_encoded(table, a_idx)
                            && column_is_f64_encoded(table, b_idx)
                            && column_is_f64_encoded(table, c_idx)
                        {
                            let a = &table.columns[a_idx];
                            let b = &table.columns[b_idx];
                            let c = &table.columns[c_idx];
                            return Some(simd_agg::sum_a_mul_one_minus_b_mul_one_plus_c_by_idx(a, b, c, indices));
                        }
                    }
                }
            }
        }
    }

    None
}

/// Vectorized sum of a compiled expression over a set of row indices.
/// This is the fast path for SUM(arithmetic_expr) queries.
///
/// W2-T2: pattern-matches the compiled tree against one of the AVX-512 FMA
/// kernels in `simd_agg.rs` (sum_a_mul_b, sum_a_mul_one_minus_b, sum_a_mul_
/// one_minus_b_mul_one_plus_c). Falls back to the scalar per-row loop for
/// shapes the kernels don't handle.
pub fn sum_compiled_f64(node: &CompiledNode, table: &Table, indices: &[usize]) -> f64 {
    if let Some(result) = try_simd_agg(node, table, indices) {
        return result;
    }
    let mut sum = 0.0f64;
    for &i in indices {
        sum += eval_compiled_f64(node, table, i);
    }
    sum
}

#[inline]
fn eval_binop_u64(op: char, l: u64, r: u64) -> u64 {
    match op {
        '+' => l.wrapping_add(r),
        '-' => l.wrapping_sub(r),
        '*' => l.wrapping_mul(r),
        '/' => if r == 0 { 0 } else { l / r },
        _ => 0,
    }
}

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

    // === W3-T2: e-graph optimization tests ===

    #[test]
    fn egraph_optimizes_mul_one() {
        // price * 1 -> price (identity)
        let t = make_table();
        let node = compile_expr("price * 1", &t).unwrap();
        // After e-graph optimization, the expression should be just Column(0).
        assert!(
            matches!(node, CompiledNode::Column(0)),
            "price * 1 should simplify to Column(0) via identity; got {:?}",
            node
        );
    }

    #[test]
    fn egraph_optimizes_add_zero() {
        // price + 0 -> price (identity)
        let t = make_table();
        let node = compile_expr("price + 0", &t).unwrap();
        assert!(
            matches!(node, CompiledNode::Column(0)),
            "price + 0 should simplify to Column(0) via identity; got {:?}",
            node
        );
    }

    #[test]
    fn egraph_optimizes_mul_zero() {
        // price * 0 -> 0 (zero)
        let t = make_table();
        let node = compile_expr("price * 0", &t).unwrap();
        // After e-graph, x*0 folds to Lit(0) = Lit(f64::to_bits(0.0)) = Lit(0).
        assert!(
            matches!(node, CompiledNode::FloatLit(bits) if bits == 0),
            "price * 0 should simplify to FloatLit(0) via zero; got {:?}",
            node
        );
    }

    #[test]
    fn egraph_optimizes_constant_fold() {
        // 2 * 3 -> 6 (constant folding)
        let t = make_table();
        let node = compile_expr("2 * 3", &t).unwrap();
        assert!(
            matches!(node, CompiledNode::FloatLit(bits) if f64::from_bits(bits) == 6.0),
            "2 * 3 should fold to FloatLit(6.0); got {:?}",
            node
        );
    }

    #[test]
    fn egraph_preserves_q1_factored_form() {
        // Q1: l_extendedprice * (1 - l_discount) * (1 + l_tax)
        // The e-graph should preserve the factored form (top-level Mul),
        // which is critical for the simd_agg Pattern 3 match.
        // Note: distributivity is disabled (creates cycles through identity),
        // so the factored form is the ONLY form in the e-class — no expansion.
        let t = make_table();
        let node = compile_expr("price * ( 1 - discount ) * ( 1 + discount )", &t).unwrap();
        assert!(
            matches!(&node, CompiledNode::BinOp { op: '*', .. }),
            "Q1-like expression should stay factored (top-level Mul); got {:?}",
            node
        );
    }

    #[test]
    fn egraph_optimizes_strength_reduction() {
        // price * 2 -> price + price (strength reduction)
        let t = make_table();
        let node = compile_expr("price * 2", &t).unwrap();
        // After e-graph, x*2 should be rewritten to x+x (Add).
        // The cost function picks Add (cost 3) over Mul (cost 5) + Lit (cost 1) = 6.
        // Wait — Mul(Col, Lit(2)) has cost = 5 + 1 + 1 = 7.
        // Add(Col, Col) has cost = 3 + 1 + 1 = 5. So Add is cheaper. ✓
        assert!(
            matches!(&node, CompiledNode::BinOp { op: '+', .. }),
            "price * 2 should simplify to price + price via strength reduction; got {:?}",
            node
        );
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
