//! Expression evaluation methods for QueryInterpreter.

use rayon::prelude::*;
use crate::catalog::Catalog;
use crate::datasource::table::Table;
use crate::engine::result::{QueryResult, ResultColumn};
use crate::exec::fm_index::StringSearchColumn;
use crate::exec::bitmap::{self, Bitmap};
use crate::Error;
use fxhash::{FxHashMap, FxHashSet};
use rayon::prelude::*;

use super::types::*;
use super::{HashMap, HashSet, new_hashmap, new_hashset, new_fxhashmap, new_fxhashset};
use super::profiler::{Phase, PROFILER};

// =========================================================================
// Top-N heap (W1 Task 1.3) — O(N log K) replacement for full sort + truncate
// =========================================================================

/// Entry in the top-N BinaryHeap. Owns a small Vec of sort keys (typically
/// 1-3 f64 values) so the heap can compare entries without borrowing from
/// the parent `sort_keys` Vec.
struct TopNEntry {
    keys: Vec<(f64, bool)>,
    idx: usize,
}
impl PartialEq for TopNEntry {
    fn eq(&self, other: &Self) -> bool { self.cmp(other) == std::cmp::Ordering::Equal }
}
impl Eq for TopNEntry {}
impl PartialOrd for TopNEntry {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> { Some(self.cmp(other)) }
}
impl Ord for TopNEntry {
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        for (i, (va, asc)) in self.keys.iter().enumerate() {
            let vb = other.keys[i].0;
            let cmp = va.total_cmp(&vb);
            let cmp = if *asc { cmp } else { cmp.reverse() };
            if cmp != std::cmp::Ordering::Equal {
                return cmp;
            }
        }
        std::cmp::Ordering::Equal
    }
}

/// Choose row indices using a max-heap of size K, then sort the K survivors.
///
/// Returns `Vec<usize>` of row indices sorted ascending per the multi-key
/// comparator. When `limit` is `None`, `0`, `>= row_count`, or `>= 10_000`,
/// falls back to a full sort of all row indices (the previous behaviour).
///
/// Complexity: O(N log K) on the heap path vs O(N log N) on the full-sort path.
fn topn_indices(sort_keys: &[Vec<(f64, bool)>], row_count: usize, limit: Option<usize>) -> Vec<usize> {
    #[inline]
    fn cmp_keys(a: &[Vec<(f64, bool)>], i: usize, j: usize) -> std::cmp::Ordering {
        for (k, (va, asc)) in a[i].iter().enumerate() {
            let vb = a[j][k].0;
            let cmp = va.total_cmp(&vb);
            let cmp = if *asc { cmp } else { cmp.reverse() };
            if cmp != std::cmp::Ordering::Equal {
                return cmp;
            }
        }
        std::cmp::Ordering::Equal
    }

    let use_heap = matches!(limit, Some(l) if l > 0 && l < 10_000 && l < row_count);

    if use_heap {
        let k = limit.unwrap();
        let mut heap: std::collections::BinaryHeap<TopNEntry> =
            std::collections::BinaryHeap::with_capacity(k + 1);
        for row_idx in 0..row_count {
            let entry = TopNEntry { keys: sort_keys[row_idx].clone(), idx: row_idx };
            if heap.len() < k {
                heap.push(entry);
            } else if let Some(top) = heap.peek() {
                // Max-heap: peek() returns the LARGEST of the K smallest kept so far.
                // Replace it when the new entry is smaller.
                if entry.cmp(top) == std::cmp::Ordering::Less {
                    heap.pop();
                    heap.push(entry);
                }
            }
        }
        let mut kept: Vec<usize> = heap.into_iter().map(|e| e.idx).collect();
        kept.sort_by(|&a, &b| cmp_keys(sort_keys, a, b));
        kept
    } else {
        let mut order: Vec<usize> = (0..row_count).collect();
        // W4-T2: parallel sort with rayon for large result sets.
        // par_sort_unstable_by uses work-stealing parallelism across N threads.
        // For small result sets (≤10000 rows), the parallel overhead exceeds
        // the benefit, so we use the serial sort.
        if row_count > 100_000 {
            order.par_sort_unstable_by(|&a, &b| cmp_keys(sort_keys, a, b));
        } else {
            order.sort_by(|&a, &b| cmp_keys(sort_keys, a, b));
        }
        order
    }
}

impl<'a> QueryInterpreter<'a> {
    pub(crate) fn filter_table(&self, table: &ExecTable, indices: &[usize]) -> ExecTable {
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
    pub(crate) fn split_conjuncts(&self, where_clause: &Option<Expr2>) -> Vec<Expr2> {
        match where_clause {
            None => Vec::new(),
            Some(expr) => {
                let mut result = Vec::new();
                self.collect_conjuncts(expr, &mut result);
                result
            }
        }
    }

    pub(crate) fn collect_conjuncts(&self, expr: &Expr2, out: &mut Vec<Expr2>) {
        match expr {
            Expr2::BinOp { op: BinOp2::And, left, right } => {
                self.collect_conjuncts(left, out);
                self.collect_conjuncts(right, out);
            }
            _ => out.push(expr.clone()),
        }
    }

    /// Find which tables an expression references.
    pub(crate) fn expr_table_refs(&self, expr: &Expr2, tables: &[ExecTable]) -> HashSet<usize> {
        let mut refs = new_hashset();
        self.collect_table_refs(expr, tables, &mut refs);
        refs
    }

    pub(crate) fn collect_table_refs(&self, expr: &Expr2, tables: &[ExecTable], refs: &mut HashSet<usize>) {
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
    pub(crate) fn collect_or_branches<'b>(&self, expr: &'b Expr2, out: &mut Vec<&'b Expr2>) {
        match expr {
            Expr2::BinOp { op: BinOp2::Or, left, right } => {
                self.collect_or_branches(left, out);
                self.collect_or_branches(right, out);
            }
            _ => out.push(expr),
        }
    }

    pub(crate) fn split_conjuncts_for_or(&self, expr: &Expr2) -> Vec<Expr2> {
        let mut result = Vec::new();
        self.collect_conjuncts(expr, &mut result);
        result
    }

    /// Direct equi-join key finder (no OR handling, used by find_or_common_keys).
    pub(crate) fn resolve_from(&self, from: &[FromItem]) -> Result<ExecTable, Error> {
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

    pub(crate) fn resolve_from_item(&self, item: &FromItem) -> Result<ExecTable, Error> {
        match item {
            FromItem::Table(t) => {
                let table = self
                    .catalog
                    .get(&t.name)
                    .ok_or_else(|| Error::NotFound(format!("table '{}'", t.name)))?;
                let alias = t.alias.as_deref().unwrap_or(&t.name);
                Ok(ExecTable::from_catalog(&table, alias))
            }
            FromItem::Derived(subquery, alias) => {
                let result = self.execute(subquery)?;
                self.result_to_exec_table(&result, alias.as_deref().unwrap_or("derived"))
            }
        }
    }

    pub(crate) fn result_to_exec_table(&self, result: &QueryResult, alias: &str) -> Result<ExecTable, Error> {
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

    pub(crate) fn infer_result_type(&self, name: &str, values: &[u64]) -> ColType {
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

    pub(crate) fn expr_refs_table(&self, expr: &Expr2, table: &ExecTable) -> bool {
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

    pub(crate) fn col_in(&self, expr: &Expr2, table: &ExecTable) -> Option<usize> {
        if let Expr2::Col(name) = expr {
            table.lookup_col(name)
        } else {
            None
        }
    }

    // --- WHERE ---

    pub(crate) fn build_mask(&self, expr: &Expr2, table: &ExecTable) -> Result<Bitmap, Error> {
        // W6A-T1: profile the WHERE-clause mask build.
        let _g = PROFILER.section(Phase::FilterMask);
        // Try vectorized fast path first; fall back to per-row eval.
        // W5A-T2: mask is a packed Bitmap (1 bit/row) instead of Vec<bool>
        // (1 byte/row). The leaf comparison fast paths in `apply_comparison`
        // already produce a Bitmap via `bitmap::filter_*` — composing
        // directly with `Bitmap::and_inplace` removes the prior
        // `and_into_bool` expansion (8x memory bandwidth) on every leaf.
        let mut mask = Bitmap::all_ones(table.row_count);
        self.eval_bool_mask_vec(expr, table, &mut mask)?;
        Ok(mask)
    }

    // --- W1-D: Q7 nation-pair LUT fast path ---

    /// Flatten an OR tree (left-associative `OR(OR(a, b), c)`) into a list
    /// of disjunct leaf expressions (by reference).
    pub(crate) fn flatten_disjuncts<'e>(expr: &'e Expr2, out: &mut Vec<&'e Expr2>) {
        match expr {
            Expr2::BinOp { op: BinOp2::Or, left, right } => {
                Self::flatten_disjuncts(left, out);
                Self::flatten_disjuncts(right, out);
            }
            _ => out.push(expr),
        }
    }

    /// Flatten an AND tree into a list of conjunct leaf expressions (by reference).
    pub(crate) fn flatten_conjuncts<'e>(expr: &'e Expr2, out: &mut Vec<&'e Expr2>) {
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
    pub(crate) fn extract_col_str_eq(expr: &Expr2) -> Option<(&str, &str)> {
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
    pub(crate) fn try_nation_pair_or_lut(
        &self,
        or_expr: &Expr2,
        t: &ExecTable,
        mask: &mut Bitmap,
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
                // mask[i] = mask[i] && bm.get(i)  (AVX-512BW bitwise AND)
                mask.and_inplace(&bm);
            }
        } else {
            // FxHashSet fallback for large N (no current TPC-H query hits
            // this; kept for correctness on hypothetical future queries).
            let set: FxHashSet<(u64, u64)> = pairs.iter().copied().collect();
            for i in 0..n {
                if !mask.get(i) {
                    continue;
                }
                let key = (col_a_data[i], col_b_data[i]);
                if !set.contains(&key) {
                    mask.clear(i);
                }
            }
        }

        Ok(true)
    }

    /// Vectorized boolean mask evaluation. Resolves column indices once,
    /// then loops over rows with direct array access. Falls back to
    /// per-row eval() for expression shapes it doesn't recognize.
    pub(crate) fn eval_bool_mask_vec(
        &self,
        expr: &Expr2,
        t: &ExecTable,
        mask: &mut Bitmap,
    ) -> Result<(), Error> {
        match expr {
            Expr2::BinOp { op: BinOp2::And, left, right } => {
                // W2: evaluate left then right directly into the same mask.
                // All leaf comparisons AND into the mask in place (via
                // `Bitmap::and_inplace`), the OR arm has been fixed to
                // AND its disjunction into the mask, and the per-row
                // fallback paths still early-exit on `if !mask.get(i) { continue; }`
                // so rows already filtered out by the left side are
                // skipped on the right side. This eliminates the previous
                // `mask.to_vec()` allocation (6 MB for a 6 M-row lineitem
                // scan) per conjunct.
                // W5A-T2: mask is a packed Bitmap; `and_inplace` uses the
                // AVX-512BW kernel (64 bytes/iter) directly on the packed
                // bytes, removing the prior `and_into_bool` byte expansion.
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
                // W2/W5A-T2: generic OR fallback. Each arm recursively fills
                // its own all-ones Bitmap (lmask / rmask), then the
                // disjunction is AND-ed into the incoming mask in place.
                // W5A-T5 will replace these two allocations with a
                // thread-local Bitmap pool; for now `Bitmap::all_ones(n)` is
                // a single ~n/8-byte allocation (8x smaller than the prior
                // `vec![true; N]`).
                let n = t.row_count;
                let mut lmask = Bitmap::all_ones(n);
                self.eval_bool_mask_vec(left, t, &mut lmask)?;
                let mut rmask = Bitmap::all_ones(n);
                self.eval_bool_mask_vec(right, t, &mut rmask)?;
                // mask &= (lmask | rmask) — two AVX-512BW word ops, no row loop.
                lmask.or_inplace(&rmask);
                mask.and_inplace(&lmask);
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
                    // W5A-T2: AND the packed bitmap directly into the
                    // running mask via AVX-512BW bitwise-and (was
                    // `and_into_bool` byte expansion).
                    mask.and_inplace(&bm);
                    Ok(())
                } else {
                    // Fallback: per-row eval
                    for i in 0..t.row_count {
                        if !mask.get(i) {
                            continue;
                        }
                        let v = self.eval(expr, t, i)?;
                        let lo = self.eval(low, t, i)?;
                        let hi = self.eval(high, t, i)?;
                        let in_range = self.cmp_le(&lo, &v) && self.cmp_le(&v, &hi);
                        if *negated == in_range {
                            mask.clear(i);
                        }
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
                        if !mask.get(i) {
                            continue;
                        }
                        let v = col[i];
                        let found = vals.iter().any(|&x| x == v);
                        if *negated == found {
                            mask.clear(i);
                        }
                    }
                    Ok(())
                } else {
                    for i in 0..t.row_count {
                        if !mask.get(i) {
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
                        if *negated == found {
                            mask.clear(i);
                        }
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
                                // W5A-T3: like_mask returns a packed Bitmap directly;
                                // AND it into the running mask with a single AVX-512BW
                                // word-wise pass (no per-row loop). For NOT LIKE we
                                // invert the bitmap first via `Bitmap::not()`.
                                // `Bitmap::and_inplace` operates on `min(self, other)`
                                // bytes, so when `sc.len() > t.row_count` the extra
                                // bits in `lm` are simply ignored.
                                let lm = self.like_mask(sc, &pat);
                                let lm = if *negated { lm.not() } else { lm };
                                mask.and_inplace(&lm);
                                return Ok(());
                            }
                        }
                    }
                }
                // Fallback: per-row eval
                for i in 0..t.row_count {
                    if !mask.get(i) {
                        continue;
                    }
                    let v = self.eval(expr, t, i)?;
                    let pv = self.eval(pattern, t, i)?;
                    let r = match (&v, &pv) {
                        (Value2::Str(s), Value2::Str(p)) => self.like(s, p),
                        _ => false,
                    };
                    if *negated == r {
                        mask.clear(i);
                    }
                }
                Ok(())
            }
            Expr2::Exists { query, negated } => {
                // W6B-T1: Vectorized EXISTS probe. Replaces the per-row
                // `self.eval(expr, t, i)` catchall (which called the
                // scalar `Expr2::Exists` arm once per outer row, each
                // doing a RefCell borrow + HashMap lookup + HashSet scan)
                // with a single bulk pass that borrows the cache ONCE and
                // iterates only the still-set mask bits via
                // `Bitmap::iter_set_bits`. For Q21 (~80k outer rows x
                // 2 EXISTS subqueries = 160k scalar probes), this eliminates
                // the per-row RefCell/AST-key/cache-lookup overhead.
                //
                // The one-time build (build_exists_hashset /
                // build_exists_multi_map) is separately profiled in
                // subquery.rs; the `_g` guard here captures the new
                // vectorized probe cost (replacing the prior per-row probe
                // cost captured by the scalar `eval` Exists arm).
                let _g = PROFILER.section(Phase::Exists);
                let ast_key = (query.as_ref() as *const SelectQuery2) as usize;

                // ---- Multi-column fast path (Q21 path: equi-join + inequality) ----
                // Pattern: EXISTS (SELECT * FROM inner WHERE inner.k = outer.k
                // AND inner.v <> outer.v). Build HashMap<equi_key,
                // HashSet<ineq_val>> once, then per outer row: look up the
                // equi_key and check if any ineq_val differs from outer's.
                if let Some((outer_eq_idx, inner_eq_idx, outer_neq_idx, inner_neq_idx)) =
                    self.find_exists_multi_col(query, t)
                {
                    let need_build = !self.exists_multi_cache.borrow().contains_key(&ast_key);
                    if need_build {
                        let map =
                            self.build_exists_multi_map(query, inner_eq_idx, inner_neq_idx)?;
                        self.exists_multi_cache.borrow_mut().insert(ast_key, map);
                    }
                    // Borrow the cache ONCE for the whole probe loop.
                    let cache = self.exists_multi_cache.borrow();
                    if let Some(map) = cache.get(&ast_key) {
                        let outer_eq_col: &[u64] = &t.columns[outer_eq_idx];
                        let outer_neq_col: &[u64] = &t.columns[outer_neq_idx];
                        // W10-T1: Parallel EXISTS probe. The serial loop was
                        // 83% of Q21's runtime (1772ms). Split the set bits
                        // into chunks, each thread probes independently, then
                        // merge the to_clear lists. The map and columns are
                        // read-only (Sync), so sharing across threads is safe.
                        //
                        // W31-T1: each entry is now an `ExistsSummary` (16 B)
                        // rather than a heap-allocated `FxHashSet<u64>`. The
                        // probe is `s.exists(outer_neq)` — a single HashMap
                        // lookup + 2 field reads, no HashSet iteration. This
                        // removes the per-group HashSet allocation (~1.5M for
                        // Q21's orderkeys) and the per-probe HashSet contains()
                        // SipHash cost.
                        let indices: Vec<usize> = mask.iter_set_bits().collect();
                        let neg = *negated;
                        let to_clear: Vec<usize> = indices
                            .par_iter()
                            .chunks(65536)
                            .map(|chunk| {
                                let mut local = Vec::new();
                                for &i in chunk {
                                    let outer_eq = outer_eq_col[i];
                                    let outer_neq = outer_neq_col[i];
                                    let exists = match map.get(outer_eq) {
                                        None => false,
                                        Some(s) => s.exists(outer_neq),
                                    };
                                    let pass = if neg { !exists } else { exists };
                                    if !pass {
                                        local.push(i);
                                    }
                                }
                                local
                            })
                            .flatten()
                            .collect();
                        // Release the cache borrow before mutating `mask`
                        // (not strictly required — different RefCells — but
                        // makes the borrow scope explicit).
                        drop(cache);
                        for i in to_clear {
                            mask.clear(i);
                        }
                        return Ok(());
                    }
                }

                // ---- Fallback: per-row correlated subquery execution ----
                // Neither fast path applied (e.g. EXISTS with a non-equi
                // correlation or 3+ correlation columns). Defer to the
                // scalar `eval` Exists arm, which sets up the outer
                // context and re-executes the subquery per row.
                for i in 0..t.row_count {
                    if !mask.get(i) {
                        continue;
                    }
                    let v = self.eval(expr, t, i)?;
                    if !self.truthy(&v) {
                        mask.clear(i);
                    }
                }
                Ok(())
            }
            Expr2::Not(inner) => {
                // W7-T3: Vectorized NOT — evaluate inner into a temp bitmap,
                // invert it, AND into the running mask. Replaces the per-row
                // fallback (100M eval() calls for Q31's NOT (CounterID = 500)).
                let n = t.row_count;
                let mut inner_mask = Bitmap::all_ones(n);
                self.eval_bool_mask_vec(inner, t, &mut inner_mask)?;
                let inverted = inner_mask.not();
                mask.and_inplace(&inverted);
                Ok(())
            }
            Expr2::InSubquery { expr, query, negated } => {
                // W13-T1: Vectorized IN-subquery. Previously this fell through
                // to the per-row path (100M eval() calls for Q04). Now we
                // build the HashSet once (cached by AST pointer), then probe
                // in bulk with rayon parallelism.
                //
                // W32-T1: Added bloom filter fast path. For ClickBench Q04
                // (100M rows, ~100k matching CounterIDs), the HashSet probe
                // was the bottleneck. The bloom filter skips the HashSet
                // lookup for ~99% of rows (non-matches), reducing probe time
                // from ~900ms to ~200ms.
                let ast_key = (query.as_ref() as *const SelectQuery2) as usize;
                let need_build = !self.in_subquery_cache.borrow().contains_key(&ast_key);
                if need_build {
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
                            self.in_subquery_cache.borrow_mut().insert(ast_key, new_fxhashset());
                        }
                    }
                }
                let cache = self.in_subquery_cache.borrow();
                if let Some(set) = cache.get(&ast_key) {
                    if !set.is_empty() || self.outer.get().is_none() {
                        // Uncorrelated — bulk probe.
                        if let Some(ci) = self.col_in(expr, t) {
                            let col = &t.columns[ci];
                            let neg = *negated;
                            // W32-T1: Build a bloom filter from the set for
                            // fast negative checks. This avoids the (slower)
                            // FxHashSet SipHash lookup for non-matching rows.
                            let mut bloom = crate::exec::bloom_filter::BloomFilter::new(set.len().max(1));
                            for &v in set.iter() {
                                bloom.insert(v);
                            }
                            // W32-T1: Batch bloom probe. Process 8 keys at a
                            // time via might_contain_batch (AVX-512F), then
                            // only check the HashSet for rows that pass the
                            // bloom. This reduces per-row overhead from ~10
                            // cycles (scalar might_contain) to ~2.5 cycles
                            // (batched). For Q04's 100M rows, this saves
                            // ~700ms.
                            let indices: Vec<usize> = mask.iter_set_bits().collect();
                            let has_avx512 = crate::exec::simd_agg::has_avx512f();
                            let to_clear: Vec<usize> = indices
                                .par_iter()
                                .chunks(65536)
                                .map(|chunk| {
                                    let mut local = Vec::new();
                                    let mut idx = 0;
                                    // Batch bloom probe: 8 keys at a time.
                                    while idx + 8 <= chunk.len() {
                                        let batch = [
                                            col[*chunk[idx]], col[*chunk[idx+1]], col[*chunk[idx+2]], col[*chunk[idx+3]],
                                            col[*chunk[idx+4]], col[*chunk[idx+5]], col[*chunk[idx+6]], col[*chunk[idx+7]],
                                        ];
                                        let bmask = if has_avx512 {
                                            unsafe { bloom.might_contain_batch(&batch) }
                                        } else {
                                            let mut m = 0u8;
                                            for j in 0..8 {
                                                if bloom.might_contain(batch[j]) { m |= 1 << j; }
                                            }
                                            m
                                        };
                                        for j in 0..8 {
                                            let i = *chunk[idx + j];
                                            let found = if (bmask >> j) & 1 == 0 {
                                                false
                                            } else {
                                                set.contains(&col[i])
                                            };
                                            let pass = if neg { !found } else { found };
                                            if !pass {
                                                local.push(i);
                                            }
                                        }
                                        idx += 8;
                                    }
                                    // Remaining keys (< 8).
                                    while idx < chunk.len() {
                                        let i = *chunk[idx];
                                        let found = if !bloom.might_contain(col[i]) {
                                            false
                                        } else {
                                            set.contains(&col[i])
                                        };
                                        let pass = if neg { !found } else { found };
                                        if !pass {
                                            local.push(i);
                                        }
                                        idx += 1;
                                    }
                                    local
                                })
                                .flatten()
                                .collect();
                            drop(cache);
                            for i in to_clear {
                                mask.clear(i);
                            }
                            return Ok(());
                        }
                        // expr is not a simple column — fall through to per-row.
                    }
                }
                drop(cache);
                // Correlated or complex — fall through to per-row.
                for i in 0..t.row_count {
                    if !mask.get(i) { continue; }
                    let v = self.eval(expr, t, i)?;
                    let v_u64 = v.to_u64();
                    // Check cache (may be correlated).
                    let cache = self.in_subquery_cache.borrow();
                    let found = if let Some(set) = cache.get(&ast_key) {
                        set.contains(&v_u64)
                    } else {
                        false
                    };
                    drop(cache);
                    let pass = if *negated { !found } else { found };
                    if !pass { mask.clear(i); }
                }
                Ok(())
            }
            _ => {
                // Fallback: per-row eval for unrecognized shapes
                for i in 0..t.row_count {
                    if !mask.get(i) {
                        continue;
                    }
                    let v = self.eval(expr, t, i)?;
                    if !self.truthy(&v) {
                        mask.clear(i);
                    }
                }
                Ok(())
            }
        }
    }

    /// Evaluate a constant expression (literal or column-independent).
    pub(crate) fn eval_const(&self, expr: &Expr2, t: &ExecTable) -> Result<Value2, Error> {
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
    ///
    /// W5A-T3: returns a packed `Bitmap` (1 bit/row) instead of `Vec<bool>`.
    /// The caller (`eval_bool_mask_vec` LIKE arm) composes it directly with
    /// `Bitmap::and_inplace` — an AVX-512BW word-wise pass that replaces the
    /// prior per-row `mask[i] = mask[i] && lm[i]` scalar loop.
    pub(crate) fn like_mask(
        &self,
        sc: &crate::exec::fm_index::StringSearchColumn,
        pattern: &str,
    ) -> Bitmap {
        let n = sc.len();
        let mut mask = Bitmap::new(n);
        if pattern.is_empty() {
            return Bitmap::all_ones(n);
        }
        let pb = pattern.as_bytes();
        if pb[0] == b'%' && !pb[1..].contains(&b'%') && !pattern.contains('_') {
            // Suffix match: %suffix
            let suffix = &pattern[1..];
            for i in 0..n {
                if sc.get(i).ends_with(suffix) {
                    mask.set(i);
                }
            }
        } else if !pattern.contains('%') && !pattern.contains('_') {
            // Exact match
            for i in 0..n {
                if sc.get(i) == pattern {
                    mask.set(i);
                }
            }
        } else if pb.len() >= 2 && pb[0] == b'%' && pb[pb.len() - 1] == b'%' && !pb[1..pb.len()-1].contains(&b'%') {
            // W22-T1/W26-T1: Flat-buffer LIKE contains — whole-buffer memchr scan.
            // Builds a flat byte buffer (all strings concatenated) + offsets,
            // then scans the entire buffer with a single memchr::Finder pass.
            // This replaces 100M random-access pointer chases (Vec<String> →
            // heap allocation per string) with one sequential scan.
            //
            // W25-T2 BUGFIX: Previously this branch did
            // `mask.and_inplace(&flat_mask)` where `mask` was initialized
            // to `Bitmap::new(n)` (all zeros). AND-anything-with-zeros =
            // zeros, so every `%substring%` LIKE filter returned 0 matches.
            // Fix: assign flat_mask directly to mask.
            //
            // W26-T1: Removed the `!pattern.contains('_')` guard. Patterns
            // like `%page_1%` (ClickBench Q21/Q22) were falling through to
            // the per-row `self.like()` path (100M calls = 2.5s). Now we use
            // the flat buffer scan for candidate detection (treating `_` as
            // a literal underscore), then verify each candidate with the full
            // LIKE pattern if the substring contains `_`. This gives correct
            // results (no false positives) while still getting the ~50x
            // speedup from the flat buffer scan. For patterns without `_`,
            // the flat buffer result is already correct — no verification
            // needed.
            let substring = &pattern[1..pb.len()-1];
            if !substring.contains('_') {
                // No wildcard in substring — flat buffer result is exact.
                mask = sc.like_contains_mask_flat(substring);
            } else {
                // W26-T1: Has `_` wildcard — use flat buffer for candidate
                // detection (treating `_` as literal underscore), then verify
                // each candidate with the full LIKE pattern. This is faster
                // than the general per-row path because the flat buffer scan
                // narrows from 100M rows to a much smaller candidate set.
                // For ClickBench Q21 (%page_1%): 100M → ~10M candidates →
                // 10M per-row LIKE checks (vs 100M without candidate filtering).
                let candidates = sc.like_contains_mask_flat(substring);
                for i in candidates.iter_set_bits() {
                    if self.like(sc.get(i), pattern) {
                        mask.set(i);
                    }
                }
            }
        } else {
            // General LIKE
            for i in 0..n {
                if self.like(sc.get(i), pattern) {
                    mask.set(i);
                }
            }
        }
        mask
    }

    /// Vectorized comparison: Col op Literal (or Literal op Col).
    /// Resolves column index once, then loops.
    /// Falls back to per-row eval for Col op Col or complex expressions.
    pub(crate) fn eval_comparison_vec(
        &self,
        op: BinOp2,
        left: &Expr2,
        right: &Expr2,
        t: &ExecTable,
        mask: &mut Bitmap,
    ) -> Result<(), Error> {
        // W14-T1: Vectorized (Col % Lit) op Lit — e.g. WatchID % 2 = 0.
        // Previously fell through to per-row eval (100M calls for Q19).
        if let Expr2::BinOp { op: BinOp2::Mod, left: mleft, right: mright } = left {
            if let Some(col_idx) = self.col_in(mleft, t) {
                if !self.expr_has_col(mright) {
                    let mval = self.eval_const(mright, t)?;
                    let rval = self.eval_const(right, t)?;
                    // Parallel vectorized modulo comparison.
                    let col = &t.columns[col_idx];
                    let m_u64 = mval.as_u64().unwrap_or(0);
                    let r_u64 = rval.as_u64().unwrap_or(0);
                    if m_u64 == 0 {
                        return Ok(()); // mod 0 — no matches
                    }
                    use rayon::prelude::*;
                    let n = t.row_count;
                    let to_clear: Vec<usize> = (0..n)
                        .into_par_iter()
                        .chunks(65536)
                        .map(|chunk| {
                            let mut local = Vec::new();
                            for i in chunk {
                                let v = col[i] % m_u64;
                                let pass = match op {
                                    BinOp2::Eq => v == r_u64,
                                    BinOp2::Ne => v != r_u64,
                                    BinOp2::Lt => v < r_u64,
                                    BinOp2::Le => v <= r_u64,
                                    BinOp2::Gt => v > r_u64,
                                    BinOp2::Ge => v >= r_u64,
                                    _ => true,
                                };
                                if !pass {
                                    local.push(i);
                                }
                            }
                            local
                        })
                        .flatten()
                        .collect();
                    for i in to_clear {
                        mask.clear(i);
                    }
                    return Ok(());
                }
            }
        }

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
        //
        // W31-T3: extended to Gt/Lt/Ge/Le for Int/Date columns. Previously
        // these fell through to the per-row `eval()` fallback (~30 ns/row →
        // 180 ms for 6 M rows). Q21 hits this path TWICE:
        //   1. Outer WHERE: `l1.l_receiptdate > l1.l_commitdate` on lineitem.
        //   2. EXISTS-2 subquery filter: `l3.l_receiptdate > l3.l_commitdate`.
        // The serial loop is ~18 ms for 6 M rows (10× speedup). Uses signed
        // i64 comparison for Int (correct for negative values); Date values
        // are always positive YYYYMMDD, so signed/unsigned agree. String is
        // excluded (hash comparison gives no meaningful ordering).
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
                            if mask.get(i) && lcol[i] != rcol[i] {
                                mask.clear(i);
                            }
                        }
                        return Ok(());
                    }
                    BinOp2::Ne => {
                        for i in 0..n {
                            if mask.get(i) && lcol[i] == rcol[i] {
                                mask.clear(i);
                            }
                        }
                        return Ok(());
                    }
                    BinOp2::Gt => {
                        // W31-T3: signed i64 comparison for Int; Date values
                        // are positive so i64 == u64 ordering.
                        if matches!(lt, ColType::Int | ColType::Date) {
                            for i in 0..n {
                                if mask.get(i) && (lcol[i] as i64) <= (rcol[i] as i64) {
                                    mask.clear(i);
                                }
                            }
                            return Ok(());
                        }
                    }
                    BinOp2::Lt => {
                        if matches!(lt, ColType::Int | ColType::Date) {
                            for i in 0..n {
                                if mask.get(i) && (lcol[i] as i64) >= (rcol[i] as i64) {
                                    mask.clear(i);
                                }
                            }
                            return Ok(());
                        }
                    }
                    BinOp2::Ge => {
                        if matches!(lt, ColType::Int | ColType::Date) {
                            for i in 0..n {
                                if mask.get(i) && (lcol[i] as i64) < (rcol[i] as i64) {
                                    mask.clear(i);
                                }
                            }
                            return Ok(());
                        }
                    }
                    BinOp2::Le => {
                        if matches!(lt, ColType::Int | ColType::Date) {
                            for i in 0..n {
                                if mask.get(i) && (lcol[i] as i64) > (rcol[i] as i64) {
                                    mask.clear(i);
                                }
                            }
                            return Ok(());
                        }
                    }
                    _ => {}
                }
            }
        }
        // Fallback: per-row eval for Col op Col or complex expressions
        for i in 0..t.row_count {
            if !mask.get(i) {
                continue;
            }
            let lv = self.eval(left, t, i)?;
            let rv = self.eval(right, t, i)?;
            let result = self.binop(op, &lv, &rv);
            if !self.truthy(&result) {
                mask.clear(i);
            }
        }
        Ok(())
    }

    /// Apply a comparison (Col op Value) to the mask vectorized.
    pub(crate) fn apply_comparison(
        &self,
        op: BinOp2,
        col_idx: usize,
        val: &Value2,
        t: &ExecTable,
        mask: &mut Bitmap,
        _negated: bool,
    ) -> Result<(), Error> {
        use crate::exec::bitmap;
        let col: &[u64] = &t.columns[col_idx];
        let col_type = t.col_types[col_idx];
        let n = t.row_count;
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
                // W5A-T2: AVX-512BW bitwise AND on packed bytes
                // (was `and_into_bool` byte expansion).
                mask.and_inplace(&bm);
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
                mask.and_inplace(&bm);
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
                mask.and_inplace(&bm);
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
                mask.and_inplace(&bm);
            }
            (ColType::String, Value2::Str(sval)) => {
                let target_hash = xxhash_rust::xxh3::xxh3_64(sval.as_bytes());
                match op {
                    BinOp2::Eq => {
                        let bm = bitmap::filter_eq_u64(col, target_hash);
                        mask.and_inplace(&bm);
                    }
                    BinOp2::Ne => {
                        let bm = bitmap::filter_ne_u64(col, target_hash);
                        mask.and_inplace(&bm);
                    }
                    _ => {}
                }
            }
            _ => {
                // Fallback: per-row eval
                for i in 0..n {
                    if !mask.get(i) {
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
                    if !matches {
                        mask.clear(i);
                    }
                }
            }
        }
        Ok(())
    }

    pub(crate) fn eval_bool_mask(
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

    pub(crate) fn truthy(&self, v: &Value2) -> bool {
        match v {
            Value2::Int(i) => *i != 0,
            Value2::Float(f) => *f != 0.0,
            Value2::Null => false,
            _ => false,
        }
    }

    // --- Expression evaluation ---

    pub(crate) fn eval(&self, expr: &Expr2, t: &ExecTable, row: usize) -> Result<Value2, Error> {
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
                // W6A-T1: profile per-row arithmetic. Note: eval recurses,
                // so nested BinOp/Extract expressions accumulate once per
                // active guard (upper bound — see profiler.rs docs).
                let _g = PROFILER.section(Phase::ExprEval);
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
                // W6A-T1: profile EXTRACT (Q9 has EXTRACT(YEAR FROM
                // o_orderdate) on every projection row).
                let _g = PROFILER.section(Phase::ExprEval);
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
                // W6A-T1: profile EXISTS decorrelation. Wraps both the
                // one-time build (delegated to build_exists_hashset /
                // build_exists_multi_map in subquery.rs — those are
                // separately wrapped, so the build time is double-counted
                // ONCE per ast_key, which is negligible vs the per-row
                // probe cost across Q21's millions of lineitem rows) and
                // the per-row bloom+hashset probe (the Q21 hot path).
                let _g = PROFILER.section(Phase::Exists);
                // Semi-join fast path: if the subquery has a single correlation
                // column with an equi-join (e.g. `l_orderkey = o_orderkey`),
                // build a hash set of inner col values ONCE and check membership.
                // This decorrelates EXISTS, reducing ~25k subquery executions
                // (Q4) to 1 hash-set build + 25k lookups.
                let ast_key = (query.as_ref() as *const SelectQuery2) as usize;
                if let Some((outer_col_idx, inner_col_idx)) = self.find_exists_equi_join(query, t) {
                    // Build the hash set + bloom filter (cached by AST pointer)
                    let need_build = !self.exists_cache.borrow().contains_key(&ast_key);
                    if need_build {
                        let set_bloom = self.build_exists_hashset(query, inner_col_idx)?;
                        self.exists_cache.borrow_mut().insert(ast_key, set_bloom);
                    }
                    let cache = self.exists_cache.borrow();
                    if let Some((set, bloom)) = cache.get(&ast_key) {
                        let outer_val = t.columns[outer_col_idx].get(row).copied().unwrap_or(0);
                        // W2-T4: bloom filter fast path. If bloom says "no",
                        // the value is definitely NOT in the set -> skip the
                        // (slower) FxHashSet SipHash lookup.
                        let exists = if !bloom.might_contain(outer_val) {
                            false
                        } else {
                            set.contains(&outer_val)
                        };
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
                        // W31-T1: probe the ExistsSummary instead of iterating
                        // a HashSet. O(1) per row: one HashMap lookup + 2 reads.
                        let exists = map
                            .get(outer_eq)
                            .map_or(false, |s| s.exists(outer_neq));
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

    pub(crate) fn binop(&self, op: BinOp2, lv: &Value2, rv: &Value2) -> Value2 {
        match op {
            BinOp2::Add | BinOp2::Sub | BinOp2::Mul | BinOp2::Div | BinOp2::Mod => {
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
                            BinOp2::Mod => {
                                if r == 0.0 {
                                    return Value2::Null;
                                }
                                l % r
                            }
                            _ => unreachable!(),
                        };
                        // Keep as int if both are ints and op is not div/mod
                        if matches!(lv, Value2::Int(_))
                            && matches!(rv, Value2::Int(_))
                            && op != BinOp2::Div
                            && op != BinOp2::Mod
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
                        // For Mod on integers, return integer result.
                        if matches!(lv, Value2::Int(_))
                            && matches!(rv, Value2::Int(_))
                            && op == BinOp2::Mod
                        {
                            let li = lv.as_i64().unwrap();
                            let ri = rv.as_i64().unwrap();
                            if ri == 0 {
                                return Value2::Null;
                            }
                            return Value2::Int(li % ri);
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

    pub(crate) fn cmp_eq(&self, a: &Value2, b: &Value2) -> bool {
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
    pub(crate) fn cmp_lt(&self, a: &Value2, b: &Value2) -> bool {
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
    pub(crate) fn cmp_le(&self, a: &Value2, b: &Value2) -> bool {
        self.cmp_lt(a, b) || self.cmp_eq(a, b)
    }

    pub(crate) fn like(&self, s: &str, pattern: &str) -> bool {
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

    pub(crate) fn extract(&self, field: &str, v: &Value2) -> Value2 {
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
    pub(crate) fn cast_value(&self, v: &Value2, target_type: &str) -> Value2 {
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

    pub(crate) fn substr(&self, s: &Value2, start: &Value2, len: &Value2) -> Value2 {
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
    pub(crate) fn expr_name(&self, expr: &Expr2) -> String {
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

    pub(crate) fn project(
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

    pub(crate) fn apply_order_by(
        &self,
        result: QueryResult,
        order_by: &[(Expr2, bool)],
        t: &ExecTable,
        indices: &[usize],
        limit: Option<usize>,
    ) -> Result<QueryResult, Error> {
        if order_by.is_empty() || result.row_count <= 1 {
            return Ok(result);
        }
        // W6A-T1: profile the sort phase (key extraction + top-N heap +
        // column reorder). Excludes the trivial early-return path above.
        let _g = PROFILER.section(Phase::Sort);
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
        // W1 Task 1.3: top-N heap when limit is small. Returns either the
        // K smallest indices (heap path) or all row_count indices (full sort).
        let order = topn_indices(&sort_keys, result.row_count, limit);
        let new_row_count = order.len();
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
        Ok(QueryResult { columns: new_cols, row_count: new_row_count, elapsed_us: 0 })
    }

    pub(crate) fn apply_order_by_grouped(
        &self,
        result: QueryResult,
        order_by: &[(Expr2, bool)],
        limit: Option<usize>,
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
        // W1 Task 1.3: top-N heap when limit is small.
        let order = topn_indices(&sort_keys, result.row_count, limit);
        let new_row_count = order.len();
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
        Ok(QueryResult { columns: new_cols, row_count: new_row_count, elapsed_us: 0 })
    }

    /// Check if an expression contains any column references.
    pub(crate) fn expr_has_col(&self, e: &Expr2) -> bool {
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
}
