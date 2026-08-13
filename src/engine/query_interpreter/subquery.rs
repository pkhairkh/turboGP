//! Subquery decorrelation and execution methods.

use crate::catalog::Catalog;
use crate::datasource::table::Table;
use crate::engine::result::{QueryResult, ResultColumn};
use crate::exec::bitmap::Bitmap;
use crate::exec::fm_index::StringSearchColumn;
use crate::Error;
use fxhash::{FxHashMap, FxHashSet};
use rayon::prelude::*;

use super::types::*;
use super::{HashMap, HashSet, new_hashmap, new_hashset, new_fxhashmap, new_fxhashset};
use super::profiler::{Phase, PROFILER};

impl<'a> QueryInterpreter<'a> {
    pub(crate) fn precache_subqueries(&self, expr: &Expr2) {
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
    pub(crate) fn find_correlation_cols(&self, subquery: &SelectQuery2, outer_t: &ExecTable) -> Vec<usize> {
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

    pub(crate) fn collect_corr_cols_filtered(
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
    pub(crate) fn is_conjunct_correlated_wrt_inner(
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
    pub(crate) fn try_decorrelate_subquery(
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
        // W5A-T2: `m` is a packed Bitmap.
        let mask = {
            let mut m = Bitmap::all_ones(base.row_count);
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
            if !mask.get(i) {
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

    pub(crate) fn find_exists_equi_join(
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

    pub(crate) fn collect_corr_names(
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
    pub(crate) fn collect_corr_names_qualified(
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
    pub(crate) fn find_equi_join_inner(
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
    pub(crate) fn resolve_inner_col_idx(
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
                    let exec_t = ExecTable::from_catalog(&table, alias);
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
    pub(crate) fn build_exists_hashset(
        &self,
        subquery: &SelectQuery2,
        inner_col_idx: usize,
    ) -> Result<(FxHashSet<u64>, crate::exec::bloom_filter::BloomFilter), Error> {
        // W6A-T1: profile the one-time EXISTS hash-set + bloom build.
        // The per-row probe (set.contains + bloom.might_contain) is
        // profiled separately in expr.rs::eval (Expr2::Exists arm).
        let _g = PROFILER.section(Phase::Exists);
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
        // W5A-T2: `mask` is a packed Bitmap.
        let mask = if let Some(ref wc) = subquery.where_clause {
            let conjuncts = self.split_conjuncts(&subquery.where_clause);
            let mut mask = Bitmap::all_ones(base.row_count);
            for conj in &conjuncts {
                if self.is_conjunct_correlated(conj, &base) {
                    continue;
                }
                self.eval_bool_mask_vec(conj, &base, &mut mask)?;
            }
            mask
        } else {
            Bitmap::all_ones(base.row_count)
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
                    if mask.get(i) {
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
        // W2-T4: build a BloomFilter from the same values. The bloom filter
        // is a fast negative-check path: bloom.might_contain returns false
        // -> definitely not in set -> skip the (slower) FxHashSet lookup.
        // On bloom "yes", fall back to the exact set check.
        let mut bloom = crate::exec::bloom_filter::BloomFilter::new(set.len().max(1));
        for &v in set.iter() {
            bloom.insert(v);
        }
        Ok((set, bloom))
    }

    /// Check if a conjunct references a column not in `base` (i.e. correlated).
    /// Uses table qualifiers to distinguish inner from outer columns.
    pub(crate) fn is_conjunct_correlated(&self, expr: &Expr2, base: &ExecTable) -> bool {
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
    pub(crate) fn qualifier_matches_base(&self, qualifier: &str, base: &ExecTable) -> bool {
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
    pub(crate) fn find_exists_multi_col(
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
    pub(crate) fn col_is_inner(
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

    /// Build `ExistsMultiMap` from the subquery's inner table, applying only
    /// uncorrelated conjuncts.
    ///
    /// W31-T1: replaces the prior `HashMap<u64, FxHashSet<u64>>`. Each group
    /// now stores a 16-byte `ExistsSummary` (count, first_val, has_diff)
    /// instead of a heap-allocated `FxHashSet`, and the per-row probe becomes
    /// a single HashMap lookup + 2 field reads (no HashSet iteration). For
    /// Q21's 1.5M orderkey groups across 6M lineitem rows, this also slashes
    /// build-time allocator pressure.
    ///
    /// W31-T2: direct-indexed `Vec<ExistsSummary>` fast path. When the max
    /// eq_key value is small (≤ `DIRECT_INDEX_MAX`), we skip hashing entirely
    /// and index a dense Vec by the eq_key. This eliminates:
    ///   1. The 6M per-row FxHash computations during the parallel chunk build.
    ///   2. The serial merge of ~96 chunk-local HashMaps into the final map
    ///      (~1.5M `entry()` calls on a cache-cold 12 MB HashMap).
    /// For Q21 SF=1 (l_orderkey max ≈ 1.5M, lineitem sorted by l_orderkey),
    /// the direct-indexed build is ~60 ms vs ~300 ms for the hash path.
    /// The probe is unchanged — `ExistsMultiMap::get()` dispatches to either
    /// `Vec::get` or `HashMap::get`.
    pub(crate) fn build_exists_multi_map(
        &self,
        subquery: &SelectQuery2,
        inner_eq_idx: usize,
        inner_neq_idx: usize,
    ) -> Result<ExistsMultiMap, Error> {
        // W6A-T1: profile the one-time multi-column EXISTS HashMap build.
        // Q21 hits this path (l_orderkey + l_suppkey correlation).
        let _g = PROFILER.section(Phase::Exists);
        let _dbg_start = std::time::Instant::now();
        let mut tables: Vec<ExecTable> = Vec::new();
        for item in &subquery.from {
            tables.push(self.resolve_from_item(item)?);
        }
        let base = if tables.len() == 1 {
            tables.into_iter().next().unwrap()
        } else {
            self.plan_join_dp(tables, &subquery.where_clause)?
        };
        let _dbg_resolve = _dbg_start.elapsed();
        // W2: evaluate each conjunct directly into `mask` (the simplified
        // AND/OR arms in `eval_bool_mask_vec` preserve the incoming mask).
        // W5A-T2: `mask` is a packed Bitmap.
        let mask = if let Some(ref wc) = subquery.where_clause {
            let conjuncts = self.split_conjuncts(&subquery.where_clause);
            let mut mask = Bitmap::all_ones(base.row_count);
            for conj in &conjuncts {
                if self.is_conjunct_correlated(conj, &base) {
                    continue;
                }
                self.eval_bool_mask_vec(conj, &base, &mut mask)?;
            }
            mask
        } else {
            Bitmap::all_ones(base.row_count)
        };
        let _dbg_mask = _dbg_start.elapsed() - _dbg_resolve;
        let eq_col = &base.columns[inner_eq_idx];
        let neq_col = &base.columns[inner_neq_idx];
        let n = base.row_count;

        // ---- Direct-indexed fast path ----
        // If the max eq_key fits in a small range, build a dense Vec indexed
        // by the eq_key value. No hashing, no merge. The Vec is zero-init;
        // entries with count == 0 are "absent" (handled by `get()`).
        //
        // W31-T1: Lowered DIRECT_INDEX_MAX from 50M to 2M. The direct Vec
        // is 16 bytes/entry, so 6M entries = 96MB — larger than the L3 cache
        // (32MB on this CPU). Random probe access into a 96MB array causes
        // an L3 miss per lookup, making the "fast" direct path slower than
        // the HashMap fallback. At 2M entries (32MB), the Vec fits in L3
        // and the direct path is faster. For Q21 (max_eq=6M), this routes
        // to the HashMap path which has ~1.5M entries (24MB, L3-resident).
        const DIRECT_INDEX_MAX: usize = 2_000_000; // ~32 MB cap at 16 B/entry
        let _dbg_max_start = std::time::Instant::now();
        let mut max_eq: u64 = 0;
        for i in 0..n {
            if mask.get(i) {
                let k = eq_col[i];
                if k > max_eq {
                    max_eq = k;
                }
            }
        }
        let _dbg_max = _dbg_max_start.elapsed();
        if (max_eq as usize) < DIRECT_INDEX_MAX {
            let _dbg_alloc_start = std::time::Instant::now();
            let cap = (max_eq as usize).saturating_add(1);
            let mut vec: Vec<ExistsSummary> = vec![ExistsSummary::default(); cap];
            let _dbg_alloc = _dbg_alloc_start.elapsed();
            let _dbg_build_start = std::time::Instant::now();
            // Iterate set bits via the Bitmap's bit iterator so we skip
            // masked-out rows without re-checking the mask per iteration.
            for i in mask.iter_set_bits() {
                let k = eq_col[i] as usize;
                // SAFE: k <= max_eq by construction (max_eq is the max over
                // set-bit rows), and cap == max_eq + 1.
                vec[k].add(neq_col[i]);
            }
            let _dbg_build = _dbg_build_start.elapsed();
            eprintln!(
                "[w31-dbg] build_exists_multi_map: n={}, max_eq={}, cap={}, \
                 resolve={:?}, mask={:?}, max={:?}, alloc={:?}, build={:?}",
                n, max_eq, cap, _dbg_resolve, _dbg_mask, _dbg_max, _dbg_alloc, _dbg_build
            );
            return Ok(ExistsMultiMap::Direct(vec));
        }
        eprintln!(
            "[w31-dbg] build_exists_multi_map: FALLBACK to HashMap (max_eq={}, n={})",
            max_eq, n
        );

        // ---- Fallback: parallel HashMap build (large/sparse eq_key range) ----
        // Each chunk builds a local HashMap, then merge via `ExistsSummary::merge`.
        const CHUNK_SIZE: usize = 65536;
        let num_chunks = (n + CHUNK_SIZE - 1) / CHUNK_SIZE;
        let local_maps: Vec<FxHashMap<u64, ExistsSummary>> = (0..num_chunks)
            .into_par_iter()
            .map(|chunk_idx| {
                let start = chunk_idx * CHUNK_SIZE;
                let end = std::cmp::min(start + CHUNK_SIZE, n);
                let mut local: FxHashMap<u64, ExistsSummary> = new_fxhashmap();
                for i in start..end {
                    if mask.get(i) {
                        // or_default() yields a Default summary (count == 0);
                        // `add` initializes first_val on the first call and
                        // flips has_diff on the first distinct value.
                        local.entry(eq_col[i]).or_default().add(neq_col[i]);
                    }
                }
                local
            })
            .collect();
        // Merge local maps into final map.
        let mut map: FxHashMap<u64, ExistsSummary> = new_fxhashmap();
        for local in local_maps {
            for (k, v) in local {
                map.entry(k).or_default().merge(v);
            }
        }
        Ok(ExistsMultiMap::Hashed(map))
    }
}
