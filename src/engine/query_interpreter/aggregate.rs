//! Aggregation methods for QueryInterpreter.

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
    pub(crate) fn has_agg(&self, select: &[SelectItem2]) -> bool {
        select.iter().any(|s| self.expr_has_agg(&s.expr))
    }
    pub(crate) fn expr_has_agg(&self, e: &Expr2) -> bool {
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

    pub(crate) fn try_low_card_grouped(
        &self,
        query: &SelectQuery2,
        t: &ExecTable,
        mask: &Bitmap,
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
                    if !mask.get(i) {
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
                    if !mask.get(i) {
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
            result = self.apply_order_by_grouped(result, &query.order_by, query.limit)?;
        }
        // W1 Task 1.3: apply_order_by_grouped already truncates to `limit`
        // when small (< 10_000) via the top-N heap path. For the non-heap
        // path (limit >= 10_000 or no ORDER BY), apply the truncate here.
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

    pub(crate) fn execute_grouped(
        &self,
        query: &SelectQuery2,
        t: &ExecTable,
        mask: &Bitmap,
    ) -> Result<QueryResult, Error> {
        // W6A-T1: profile the entire aggregation phase — scalar agg
        // fast path, low-cardinality fast path, and the HashMap-based
        // parallel grouping fallback. The guard drops on every return
        // path (early or final), accumulating the active duration.
        let _g = PROFILER.section(Phase::Aggregate);
        if query.group_by.is_empty() {
            // W5A-T2: `iter_set_bits()` (tzcnt) skips false rows without
            // a branch per row.
            let indices: Vec<usize> = mask.iter_set_bits().collect();
            return self.execute_scalar_agg(query, t, &indices);
        }

        // Low-cardinality fast path (Q1: 4 groups, Q13: ~40 groups)
        if let Some(result) = self.try_low_card_grouped(query, t, mask)? {
            return Ok(result);
        }

        // W7-T2: Accumulator-based grouping for simple aggregates.
        // Uses O(1)-per-group accumulators (counters, sums, mins, maxs)
        // instead of Vec<usize> per group (which is O(n) memory + O(n)
        // push calls). Handles COUNT(*), SUM, AVG, MIN, MAX, and GROUP BY
        // columns, with simple HAVING and single-column ORDER BY.
        // Falls back to the Vec<usize> path for unsupported patterns.
        if let Some(result) = self.try_accumulator_grouped(query, t, mask)? {
            return Ok(result);
        }

        // Fallback: HashMap-based grouping for high cardinality.
        // PARALLEL: split into chunks, each thread builds a local HashMap,
        // then merge. This is critical for Q3 (10k groups, 300k rows) which
        // was serial and took ~200ms just for grouping.
        let indices: Vec<usize> = mask.iter_set_bits().collect();

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
            result = self.apply_order_by_grouped(result, &query.order_by, query.limit)?;
        }
        // W1 Task 1.3: apply_order_by_grouped already truncates to `limit`
        // when small (< 10_000) via the top-N heap path. For the non-heap
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

    // Rust function to insert before execute_scalar_agg
    /// Fused per-group aggregation: analyze all select items, and if they
    /// match supported patterns, do a SINGLE pass per group computing all aggregates.
    pub(crate) fn try_fused_grouped_agg(
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

    pub(crate) fn execute_scalar_agg(
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

    pub(crate) fn eval_agg_expr(
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

    pub(crate) fn sum_vec(&self, expr: &Expr2, t: &ExecTable, indices: &[usize]) -> Result<Value2, Error> {
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
                        // W5A-T6: Vec<bool> -> Bitmap. `da`/`db` are already
                        // pre-gathered from the active indices, so the mask is
                        // all-ones; dot_f64_bf16 detects this via POPCNT and
                        // dispatches to the unmasked AVX-512 BF16 inner kernel
                        // (skips the per-element bit-extract in the hot loop).
                        let mask = Bitmap::all_ones(n);
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
    pub(crate) fn minmax_vec(
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
    pub(crate) fn col_in_sub_one_right(&self, expr: &Expr2, t: &ExecTable) -> Option<usize> {
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
    pub(crate) fn col_in_mul_sub_one(&self, expr: &Expr2, t: &ExecTable) -> Option<(usize, usize)> {
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
    pub(crate) fn col_in_mul_sub_one_add_one(&self, expr: &Expr2, t: &ExecTable) -> Option<(usize, usize)> {
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
    pub(crate) fn col_in_add_one_right(&self, expr: &Expr2, t: &ExecTable) -> Option<usize> {
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

    pub(crate) fn sum_values(&self, values: &[Value2]) -> Value2 {
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

    pub(crate) fn avg_values(&self, values: &[Value2]) -> Value2 {
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

    pub(crate) fn min_values(&self, values: &[Value2]) -> Value2 {
        let mut min: Option<f64> = None;
        for v in values {
            if let Some(f) = v.as_f64() {
                min = Some(min.map_or(f, |m| m.min(f)));
            }
        }
        min.map(Value2::Float).unwrap_or(Value2::Null)
    }

    pub(crate) fn max_values(&self, values: &[Value2]) -> Value2 {
        let mut max: Option<f64> = None;
        for v in values {
            if let Some(f) = v.as_f64() {
                max = Some(max.map_or(f, |m| m.max(f)));
            }
        }
        max.map(Value2::Float).unwrap_or(Value2::Null)
    }
}
