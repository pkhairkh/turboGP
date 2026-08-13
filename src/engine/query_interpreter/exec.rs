//! Core query execution for QueryInterpreter.

use crate::catalog::Catalog;
use crate::datasource::table::Table;
use crate::engine::result::{QueryResult, ResultColumn};
use crate::exec::bitmap::Bitmap;
use crate::exec::fm_index::StringSearchColumn;
use crate::Error;
use fxhash::{FxHashMap, FxHashSet};
use std::cell::{Cell, RefCell};
use rayon::prelude::*;

use super::types::*;
use super::{HashMap, HashSet, new_hashmap, new_hashset, new_fxhashmap, new_fxhashset};

pub fn execute_interpreter(query: &SelectQuery2, catalog: &Catalog) -> Result<QueryResult, Error> {
    QueryInterpreter {
        catalog,
        outer: std::cell::Cell::new(None),
        subquery_cache: std::cell::RefCell::new(new_hashmap()),
        exists_cache: std::cell::RefCell::new(new_hashmap()),
        exists_multi_cache: std::cell::RefCell::new(new_hashmap()),
        in_subquery_cache: std::cell::RefCell::new(new_hashmap()),
        decorrelated_cache: std::cell::RefCell::new(new_hashmap()),
        arena: crate::exec::arena::QueryArena::new(),
    }
    .execute(query)
}

impl<'a> QueryInterpreter<'a> {
    /// Access the per-query bump arena. Use for intermediate allocations
    /// (join output buffers, index lists) to avoid per-allocation malloc/free
    /// overhead. The arena is freed in one shot when the interpreter is
    /// dropped at query end.
    ///
    /// W5B-T2: `QueryArena` wraps `bumpalo::Bump` which is `Send` but not
    /// `Sync`. The interpreter is used single-threaded per query, so the
    /// shared `&self` accessor is safe. Callers inside rayon parallel
    /// sections must create their own chunk-local arenas instead of using
    /// this accessor.
    pub(crate) fn arena(&self) -> &crate::exec::arena::QueryArena {
        &self.arena
    }
    pub(crate) fn execute(&self, query: &SelectQuery2) -> Result<QueryResult, Error> {
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
                Bitmap::all_ones(base.row_count)
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
                Bitmap::all_ones(base.row_count)
            } else {
                // W2: evaluate each multi-table conjunct directly into the
                // running mask. The simplified AND arm + fixed OR arm in
                // `eval_bool_mask_vec` preserve the incoming mask (every
                // leaf ANDs into it), so the previous per-conjunct
                // `mask.clone()` (6 MB for a 6 M-row base table) is no
                // longer needed.
                // W5A-T2: mask is a packed Bitmap (1 bit/row) — 8x smaller
                // than the prior `vec![true; N]`.
                let mut mask = Bitmap::all_ones(base.row_count);
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
        // W5A-T2: `iter_set_bits()` (tzcnt-based) skips false rows without
        // a branch per row.
        let indices: Vec<usize> = mask.iter_set_bits().collect();
        let result = self.project(&query.select, &base, &indices)?;
        let mut result = if !query.order_by.is_empty() {
            self.apply_order_by(result, &query.order_by, &base, &indices, query.limit)?
        } else {
            result
        };

        // W1 Task 1.3: apply_order_by already truncates to `limit` when the
        // limit is small (< 10_000) via the top-N heap path. For the non-heap
        // path (limit >= 10_000 or no ORDER BY), apply the truncate here.
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
}
