//! W7-T2: Accumulator-based GROUP BY.
//!
//! Replaces the `Vec<usize>`-per-group approach with O(1)-per-group
//! accumulators for queries where all SELECT items are simple aggregates
//! (COUNT(*), SUM(col), AVG(col), MIN(col), MAX(col)) or GROUP BY columns.
//!
//! For Q27 (100M rows, GROUP BY 2 cols, COUNT(*)):
//! - Old: 100M `Vec::push` calls, 800MB of row indices
//! - New: ~180K groups × 24 bytes = 4.3KB of accumulators
//!
//! Also handles simple HAVING (`agg OP literal`) and ORDER BY (by alias
//! or GROUP BY column).

use crate::engine::result::{QueryResult, ResultColumn};
use crate::exec::bitmap::Bitmap;
use crate::Error;
use fxhash::FxHashMap;
use rayon::prelude::*;

use super::types::*;
use super::profiler::{Phase, PROFILER};

/// One slot in the per-group accumulator vector.
#[derive(Clone, Debug)]
enum AccSlot {
    /// A GROUP BY column value (set once at group creation).
    GroupByCol(usize),
    /// COUNT(*) — increment per row.
    CountAll,
    /// COUNT(col) — increment per non-null (non-zero) row.
    CountCol(usize),
    /// SUM(col) — running sum. `is_float` controls interpretation.
    SumCol(usize, bool),
    /// AVG(col) — 2 slots: (sum, count).
    AvgCol(usize, bool),
    /// MIN(col) — running min, initialized to u64::MAX.
    MinCol(usize),
    /// MAX(col) — running max, initialized to 0.
    MaxCol(usize),
}

impl AccSlot {
    /// Number of u64 slots this accumulator occupies.
    fn n_slots(&self) -> usize {
        match self {
            AccSlot::AvgCol(_, _) => 2,
            _ => 1,
        }
    }
}

impl<'a> QueryInterpreter<'a> {
    /// Try the accumulator-based GROUP BY path. Returns `Ok(None)` if the
    /// query shape is not supported (caller falls back to `Vec<usize>`).
    pub(crate) fn try_accumulator_grouped(
        &self,
        query: &SelectQuery2,
        t: &ExecTable,
        mask: &Bitmap,
    ) -> Result<Option<QueryResult>, Error> {
        let _g = PROFILER.section(Phase::Aggregate);

        // 1. Resolve GROUP BY column indices (must all be plain columns).
        let gb_cols: Vec<Option<usize>> =
            query.group_by.iter().map(|gb| self.col_in(gb, t)).collect();
        if gb_cols.iter().any(|c| c.is_none()) {
            return Ok(None); // computed GROUP BY expression — fall back
        }
        let gb_cols: Vec<usize> = gb_cols.iter().map(|c| c.unwrap()).collect();

        // 2. Analyze SELECT items — build accumulator plan.
        let mut slots: Vec<AccSlot> = Vec::with_capacity(query.select.len());
        for item in &query.select {
            let slot = match &item.expr {
                Expr2::CountStar => AccSlot::CountAll,
                Expr2::Agg { func, arg, distinct: false } => match func {
                    AggFunc::Count => {
                        // COUNT(*) is parsed as Agg{Count, arg=CountStar}.
                        // COUNT(col) is parsed as Agg{Count, arg=Col(name)}.
                        if matches!(arg.as_ref(), Expr2::CountStar) {
                            AccSlot::CountAll
                        } else if let Some(ci) = self.col_in(arg, t) {
                            AccSlot::CountCol(ci)
                        } else {
                            return Ok(None);
                        }
                    }
                    AggFunc::Sum => {
                        if let Some(ci) = self.col_in(arg, t) {
                            let is_float = t.col_types.get(ci).copied() == Some(ColType::Float);
                            AccSlot::SumCol(ci, is_float)
                        } else {return Ok(None);
                        }
                    }
                    AggFunc::Avg => {
                        if let Some(ci) = self.col_in(arg, t) {
                            let is_float = t.col_types.get(ci).copied() == Some(ColType::Float);
                            AccSlot::AvgCol(ci, is_float)
                        } else {return Ok(None);
                        }
                    }
                    AggFunc::Min => {
                        if let Some(ci) = self.col_in(arg, t) {
                            AccSlot::MinCol(ci)
                        } else {return Ok(None);
                        }
                    }
                    AggFunc::Max => {
                        if let Some(ci) = self.col_in(arg, t) {
                            AccSlot::MaxCol(ci)
                        } else {return Ok(None);
                        }
                    }
                    AggFunc::CountDistinct => return Ok(None), // needs HashSet — fall back
                },
                Expr2::Col(name) => {
                    // Must be a GROUP BY column.
                    if let Some(ci) = t.lookup_col(name) {
                        if gb_cols.contains(&ci) {
                            AccSlot::GroupByCol(ci)
                        } else {return Ok(None); // non-group-by col without agg — fall back
                        }
                    } else {return Ok(None);
                    }
                }
                _ => return Ok(None), // unsupported expression — fall back
            };
            slots.push(slot);
        }

        // Precompute slot offsets for each SELECT item.
        let slot_offsets: Vec<usize> = {
            let mut offs = Vec::with_capacity(slots.len());
            let mut cur = 0;
            for s in &slots {
                offs.push(cur);
                cur += s.n_slots();
            }
            offs
        };
        let slots_per_group: usize = slots.iter().map(|s| s.n_slots()).sum();

        // 3. Check HAVING — only support `slot_expr OP literal`.
        let having_filter: Option<(usize, BinOp2, i64)> = if let Some(ref having) = query.having {
            if let Expr2::BinOp { op, left, right } = having {
                let slot_idx = self.match_expr_to_slot(left, &query.select, &slots, &slot_offsets, t);
                let lit = self.eval_const_i64(right, t);
                if let (Some(idx), Some(val)) = (slot_idx, lit) {
                    Some((idx, *op, val))
                } else {return Ok(None); // complex HAVING — fall back
                }
            } else {return Ok(None);
            }
        } else {
            None
        };

        // 4. Check ORDER BY — only single-column, by alias or GROUP BY col.
        let order_spec: Option<(usize, bool)> = if query.order_by.len() == 1 {
            let (ref oe, desc) = &query.order_by[0];
            let slot_idx = self.match_expr_to_slot(oe, &query.select, &slots, &slot_offsets, t);
            slot_idx.map(|idx| (idx, *desc))
        } else if query.order_by.is_empty() {
            None
        } else {return Ok(None); // multi-column ORDER BY — fall back
        };

        // W7-T2 fast-count path: when all SELECT items are GROUP BY columns
        // or COUNT(*), use a compact HashMap<u64, (u64, u64)> (count,
        // first_row_idx) instead of HashMap<u64, Vec<u64>>. This eliminates
        // per-group Vec allocation — critical for high-cardinality GROUP BY
        // like Q27 (100M groups). The GROUP BY column values are recovered
        // from first_row_idx at the end.
        let is_pure_count = slots.iter().all(|s| {
            matches!(s, AccSlot::GroupByCol(_) | AccSlot::CountAll)
        });
        if is_pure_count {
            return self.exec_fast_count_grouped(
                query, t, mask, &gb_cols, &slots, &slot_offsets,
                having_filter, order_spec,
            );
        }

        // 5. Parallel accumulation.
        let indices: Vec<usize> = mask.iter_set_bits().collect();
        let n_indices = indices.len();
        const GROUP_CHUNK_SIZE: usize = 65536;
        let num_chunks = (n_indices + GROUP_CHUNK_SIZE - 1) / GROUP_CHUNK_SIZE;

        let local_maps: Vec<FxHashMap<u64, Vec<u64>>> = if num_chunks == 0 {
            Vec::new()
        } else {
            (0..num_chunks)
                .into_par_iter()
                .map(|chunk_idx| {
                    let start = chunk_idx * GROUP_CHUNK_SIZE;
                    let end = std::cmp::min(start + GROUP_CHUNK_SIZE, n_indices);
                    let mut local: FxHashMap<u64, Vec<u64>> = FxHashMap::default();

                    for i in start..end {
                        let idx = indices[i];
                        // Compute group key hash.
                        let mut key_hash: u64 = 0;
                        for &ci in &gb_cols {
                            let v = t.columns[ci][idx];
                            key_hash = key_hash.wrapping_mul(0x517cc1b727220a95).wrapping_add(v);
                        }

                        let acc = local.entry(key_hash).or_insert_with(|| {
                            let mut v = vec![0u64; slots_per_group];
                            // Initialize slots.
                            for (si, s) in slots.iter().enumerate() {
                                let off = slot_offsets[si];
                                match s {
                                    AccSlot::GroupByCol(ci) => v[off] = t.columns[*ci][idx],
                                    AccSlot::MinCol(_) => v[off] = u64::MAX,
                                    AccSlot::MaxCol(_) => v[off] = u64::MIN,
                                    AccSlot::AvgCol(_, _) => { /* sum=0, count=0 */ }
                                    _ => {}
                                }
                            }
                            v
                        });

                        // Update accumulators.
                        for (si, s) in slots.iter().enumerate() {
                            let off = slot_offsets[si];
                            match s {
                                AccSlot::CountAll => {
                                    acc[off] = acc[off].wrapping_add(1);
                                }
                                AccSlot::CountCol(ci) => {
                                    if t.columns[*ci][idx] != 0 {
                                        acc[off] = acc[off].wrapping_add(1);
                                    }
                                }
                                AccSlot::SumCol(ci, is_float) => {
                                    let val = t.columns[*ci][idx];
                                    if *is_float {
                                        let sum = f64::from_bits(acc[off]);
                                        let add = f64::from_bits(val);
                                        acc[off] = (sum + add).to_bits();
                                    } else {
                                        acc[off] = acc[off].wrapping_add(val);
                                    }
                                }
                                AccSlot::AvgCol(ci, is_float) => {
                                    let val = t.columns[*ci][idx];
                                    if *is_float {
                                        let sum = f64::from_bits(acc[off]);
                                        let add = f64::from_bits(val);
                                        acc[off] = (sum + add).to_bits();
                                    } else {
                                        acc[off] = acc[off].wrapping_add(val);
                                    }
                                    acc[off + 1] = acc[off + 1].wrapping_add(1);
                                }
                                AccSlot::MinCol(ci) => {
                                    let val = t.columns[*ci][idx];
                                    if val < acc[off] {
                                        acc[off] = val;
                                    }
                                }
                                AccSlot::MaxCol(ci) => {
                                    let val = t.columns[*ci][idx];
                                    if val > acc[off] {
                                        acc[off] = val;
                                    }
                                }
                                AccSlot::GroupByCol(_) => { /* set at init */ }
                            }
                        }
                    }
                    local
                })
                .collect()
        };

        // 6. Merge local maps.
        let mut global: FxHashMap<u64, Vec<u64>> = FxHashMap::default();
        for local in local_maps {
            for (hash, acc) in local {
                global.entry(hash).and_modify(|existing| {
                    for (si, s) in slots.iter().enumerate() {
                        let off = slot_offsets[si];
                        match s {
                            AccSlot::CountAll | AccSlot::CountCol(_) => {
                                existing[off] = existing[off].wrapping_add(acc[off]);
                            }
                            AccSlot::SumCol(_, is_float) => {
                                if *is_float {
                                    let s1 = f64::from_bits(existing[off]);
                                    let s2 = f64::from_bits(acc[off]);
                                    existing[off] = (s1 + s2).to_bits();
                                } else {
                                    existing[off] = existing[off].wrapping_add(acc[off]);
                                }
                            }
                            AccSlot::AvgCol(_, is_float) => {
                                if *is_float {
                                    let s1 = f64::from_bits(existing[off]);
                                    let s2 = f64::from_bits(acc[off]);
                                    existing[off] = (s1 + s2).to_bits();
                                } else {
                                    existing[off] = existing[off].wrapping_add(acc[off]);
                                }
                                existing[off + 1] = existing[off + 1].wrapping_add(acc[off + 1]);
                            }
                            AccSlot::MinCol(_) => {
                                if acc[off] < existing[off] {
                                    existing[off] = acc[off];
                                }
                            }
                            AccSlot::MaxCol(_) => {
                                if acc[off] > existing[off] {
                                    existing[off] = acc[off];
                                }
                            }
                            AccSlot::GroupByCol(_) => { /* keep existing */ }
                        }
                    }
                }).or_insert(acc);
            }
        }

        // 7. Collect groups.
        let mut groups: Vec<Vec<u64>> = global.into_values().collect();

        // 8. HAVING filter.
        if let Some((slot_idx, op, val)) = having_filter {
            groups.retain(|acc| {
                let v = acc[slot_idx] as i64;
                match op {
                    BinOp2::Gt => v > val,
                    BinOp2::Ge => v >= val,
                    BinOp2::Lt => v < val,
                    BinOp2::Le => v <= val,
                    BinOp2::Eq => v == val,
                    BinOp2::Ne => v != val,
                    _ => true,
                }
            });
        }

        // 9. ORDER BY (single column).
        if let Some((slot_idx, desc)) = order_spec {
            groups.sort_by(|a, b| {
                let va = a[slot_idx];
                let vb = b[slot_idx];
                if desc {
                    vb.cmp(&va)
                } else {
                    va.cmp(&vb)
                }
            });
        }

        // 10. LIMIT.
        if let Some(limit) = query.limit {
            if groups.len() > limit {
                groups.truncate(limit);
            }
        }

        // 11. Build result columns.
        let mut cols: Vec<ResultColumn> = Vec::with_capacity(query.select.len());
        for (item_idx, item) in query.select.iter().enumerate() {
            let name = item.alias.clone().unwrap_or_else(|| self.expr_name(&item.expr));
            let off = slot_offsets[item_idx];
            let values: Vec<u64> = groups
                .iter()
                .map(|acc| match &slots[item_idx] {
                    AccSlot::AvgCol(_, is_float) => {
                        let count = acc[off + 1];
                        if count == 0 {
                            0
                        } else if *is_float {
                            let sum = f64::from_bits(acc[off]);
                            (sum / count as f64).to_bits()
                        } else {
                            acc[off] / count
                        }
                    }
                    _ => acc[off],
                })
                .collect();

            cols.push(ResultColumn {
                name,
                values,
                string_values: None,
                type_oid: 0,
                null_mask: None,
            });
        }

        Ok(Some(QueryResult {
            columns: cols,
            row_count: groups.len(),
            elapsed_us: 0,
        }))
    }

    /// Fast-count GROUP BY path: compact (count, first_row_idx) accumulator.
    ///
    /// Used when all SELECT items are GROUP BY columns or COUNT(*).
    /// Eliminates per-group Vec allocation — for Q27 (100M groups) this
    /// saves 100M malloc/free calls and ~1.6 GB of Vec metadata.
    fn exec_fast_count_grouped(
        &self,
        query: &SelectQuery2,
        t: &ExecTable,
        mask: &Bitmap,
        gb_cols: &[usize],
        slots: &[AccSlot],
        slot_offsets: &[usize],
        having_filter: Option<(usize, BinOp2, i64)>,
        order_spec: Option<(usize, bool)>,
    ) -> Result<Option<QueryResult>, Error> {
        use rayon::prelude::*;

        let n = t.row_count;
        let count = mask.count_ones();

        // W15-T1: Sort-based GROUP BY for high-cardinality cases.
        // The HashMap-based approach struggles with 100M groups due to
        // random-access insertion and resize overhead. The sort-based
        // approach: compute (hash, row_idx) pairs, sort by hash, then
        // group consecutive equal hashes (sequential scan, cache-friendly).
        //
        // For full-mask (no WHERE), we skip the mask iteration entirely
        // and iterate the column directly.
        //
        // For low-cardinality cases (count < 1M), the HashMap approach is
        // faster (no sort overhead), so we keep the old path.

        if count > 1_000_000 {
            // Build (hash, row_idx) pairs.
            let mut pairs: Vec<(u64, u32)> = if count == n {
                // Full mask — iterate directly.
                (0..n)
                    .into_par_iter()
                    .map(|i| {
                        let mut key_hash: u64 = 0;
                        for &ci in gb_cols {
                            let v = t.columns[ci][i];
                            key_hash = key_hash.wrapping_mul(0x517cc1b727220a95).wrapping_add(v);
                        }
                        (key_hash, i as u32)
                    })
                    .collect()
            } else {
                // Filtered — iterate mask bits.
                let indices: Vec<usize> = mask.iter_set_bits().collect();
                indices
                    .par_iter()
                    .map(|&i| {
                        let mut key_hash: u64 = 0;
                        for &ci in gb_cols {
                            let v = t.columns[ci][i];
                            key_hash = key_hash.wrapping_mul(0x517cc1b727220a95).wrapping_add(v);
                        }
                        (key_hash, i as u32)
                    })
                    .collect()
            };

            // Parallel sort by hash.
            pairs.par_sort_unstable_by_key(|(h, _)| *h);

            // Group consecutive equal hashes — sequential scan.
            // Each group: (count, first_row_idx).
            let mut groups: Vec<(u64, u64)> = Vec::with_capacity(pairs.len() / 2);
            if !pairs.is_empty() {
                let mut cur_hash = pairs[0].0;
                let mut cur_count: u64 = 1;
                let mut cur_first: u64 = pairs[0].1 as u64;
                for &(h, row_idx) in &pairs[1..] {
                    if h == cur_hash {
                        cur_count += 1;
                    } else {
                        groups.push((cur_count, cur_first));
                        cur_hash = h;
                        cur_count = 1;
                        cur_first = row_idx as u64;
                    }
                }
                groups.push((cur_count, cur_first));
            }

            // Continue with HAVING / ORDER BY / LIMIT / column building
            // (same as the HashMap path below).
            return self.finish_fast_count_grouped(
                query, t, &groups, slots, slot_offsets, having_filter, order_spec,
            );
        }

        // Low-cardinality path: HashMap-based (original).
        let indices: Vec<usize> = mask.iter_set_bits().collect();
        let n_indices = indices.len();
        const GROUP_CHUNK_SIZE: usize = 65536;
        let num_chunks = (n_indices + GROUP_CHUNK_SIZE - 1) / GROUP_CHUNK_SIZE;

        // Parallel accumulation: HashMap<u64, (u64, u64)> = (count, first_row_idx).
        let local_maps: Vec<FxHashMap<u64, (u64, u64)>> = if num_chunks == 0 {
            Vec::new()
        } else {
            (0..num_chunks)
                .into_par_iter()
                .map(|chunk_idx| {
                    let start = chunk_idx * GROUP_CHUNK_SIZE;
                    let end = std::cmp::min(start + GROUP_CHUNK_SIZE, n_indices);
                    let mut local: FxHashMap<u64, (u64, u64)> = FxHashMap::default();
                    for i in start..end {
                        let idx = indices[i];
                        let mut key_hash: u64 = 0;
                        for &ci in gb_cols {
                            let v = t.columns[ci][idx];
                            key_hash = key_hash.wrapping_mul(0x517cc1b727220a95).wrapping_add(v);
                        }
                        let entry = local.entry(key_hash).or_insert((0, idx as u64));
                        entry.0 = entry.0.wrapping_add(1);
                    }
                    local
                })
                .collect()
        };

        // Merge: sum counts, keep any first_row_idx.
        let mut global: FxHashMap<u64, (u64, u64)> = FxHashMap::default();
        for local in local_maps {
            for (hash, (count, first_idx)) in local {
                global
                    .entry(hash)
                    .and_modify(|e| e.0 = e.0.wrapping_add(count))
                    .or_insert((count, first_idx));
            }
        }

        // Collect groups: Vec<(count, first_row_idx)>.
        let groups: Vec<(u64, u64)> = global.into_values().collect();

        // Shared HAVING / ORDER BY / LIMIT / column building.
        self.finish_fast_count_grouped(
            query, t, &groups, slots, slot_offsets, having_filter, order_spec,
        )
    }

    /// Shared finishing logic for the fast-count GROUP BY path:
    /// HAVING filter, ORDER BY, LIMIT, and result column construction.
    fn finish_fast_count_grouped(
        &self,
        query: &SelectQuery2,
        t: &ExecTable,
        groups: &[(u64, u64)],
        slots: &[AccSlot],
        slot_offsets: &[usize],
        having_filter: Option<(usize, BinOp2, i64)>,
        order_spec: Option<(usize, bool)>,
    ) -> Result<Option<QueryResult>, Error> {
        let mut groups: Vec<(u64, u64)> = groups.to_vec();

        // Find the CountAll slot offset (for HAVING/ORDER BY).
        let countall_offset: Option<usize> = slots.iter().enumerate().find_map(|(i, s)| {
            if matches!(s, AccSlot::CountAll) {
                Some(slot_offsets[i])
            } else {
                None
            }
        });

        // HAVING: only supports filtering by COUNT(*) (slot offset = countall_offset).
        if let Some((slot_idx, op, val)) = having_filter {
            if Some(slot_idx) == countall_offset {
                groups.retain(|(count, _)| {
                    let v = *count as i64;
                    match op {
                        BinOp2::Gt => v > val,
                        BinOp2::Ge => v >= val,
                        BinOp2::Lt => v < val,
                        BinOp2::Le => v <= val,
                        BinOp2::Eq => v == val,
                        BinOp2::Ne => v != val,
                        _ => true,
                    }
                });
            } else {
                let gb_col_idx = slots.iter().enumerate().find_map(|(i, s)| {
                    if slot_offsets[i] == slot_idx {
                        if let AccSlot::GroupByCol(ci) = s {
                            return Some(*ci);
                        }
                    }
                    None
                });
                if let Some(ci) = gb_col_idx {
                    groups.retain(|(_, first_idx)| {
                        let v = t.columns[ci][*first_idx as usize] as i64;
                        match op {
                            BinOp2::Gt => v > val,
                            BinOp2::Ge => v >= val,
                            BinOp2::Lt => v < val,
                            BinOp2::Le => v <= val,
                            BinOp2::Eq => v == val,
                            BinOp2::Ne => v != val,
                            _ => true,
                        }
                    });
                }
            }
        }

        // ORDER BY: sort by the slot referenced.
        if let Some((slot_idx, desc)) = order_spec {
            if Some(slot_idx) == countall_offset {
                groups.sort_by(|a, b| {
                    if desc { b.0.cmp(&a.0) } else { a.0.cmp(&b.0) }
                });
            } else {
                let gb_col_idx = slots.iter().enumerate().find_map(|(i, s)| {
                    if slot_offsets[i] == slot_idx {
                        if let AccSlot::GroupByCol(ci) = s {
                            return Some(*ci);
                        }
                    }
                    None
                });
                if let Some(ci) = gb_col_idx {
                    groups.sort_by(|a, b| {
                        let va = t.columns[ci][a.1 as usize];
                        let vb = t.columns[ci][b.1 as usize];
                        if desc { vb.cmp(&va) } else { va.cmp(&vb) }
                    });
                }
            }
        }

        // LIMIT.
        if let Some(limit) = query.limit {
            if groups.len() > limit {
                groups.truncate(limit);
            }
        }

        // Build result columns.
        let mut cols: Vec<ResultColumn> = Vec::with_capacity(query.select.len());
        for (item_idx, item) in query.select.iter().enumerate() {
            let name = item.alias.clone().unwrap_or_else(|| self.expr_name(&item.expr));
            let off = slot_offsets[item_idx];
            let values: Vec<u64> = groups
                .iter()
                .map(|(count, first_idx)| {
                    match &slots[item_idx] {
                        AccSlot::CountAll => *count,
                        AccSlot::GroupByCol(ci) => t.columns[*ci][*first_idx as usize],
                        _ => 0,
                    }
                })
                .collect();
            let _ = off;
            cols.push(ResultColumn {
                name,
                values,
                string_values: None,
                type_oid: 0,
                null_mask: None,
            });
        }

        Ok(Some(QueryResult {
            columns: cols,
            row_count: groups.len(),
            elapsed_us: 0,
        }))
    }

    /// Map an expression (from HAVING or ORDER BY) to a slot index in the
    /// accumulator vector. Returns `None` if the expression doesn't match
    /// any SELECT item or GROUP BY column.
    fn match_expr_to_slot(
        &self,
        expr: &Expr2,
        select: &[SelectItem2],
        slots: &[AccSlot],
        slot_offsets: &[usize],
        t: &ExecTable,
    ) -> Option<usize> {
        // 1. If expr is a Col matching a SELECT alias, use that item's slot.
        if let Expr2::Col(name) = expr {
            for (i, item) in select.iter().enumerate() {
                if item.alias.as_deref() == Some(name.as_str()) {
                    return Some(slot_offsets[i]);
                }
            }
        }

        // 2. If expr matches a SELECT item's expression structurally, use it.
        for (i, item) in select.iter().enumerate() {
            if exprs_equal(&item.expr, expr) {
                return Some(slot_offsets[i]);
            }
        }

        // 3. If expr is a Col matching a GROUP BY column, find the GroupByCol slot.
        if let Expr2::Col(name) = expr {
            if let Some(ci) = t.lookup_col(name) {
                for (i, s) in slots.iter().enumerate() {
                    if let AccSlot::GroupByCol(gci) = s {
                        if *gci == ci {
                            return Some(slot_offsets[i]);
                        }
                    }
                }
            }
        }

        None
    }

    /// Evaluate a constant expression to i64. Returns None for non-constant.
    fn eval_const_i64(&self, expr: &Expr2, _t: &ExecTable) -> Option<i64> {
        match expr {
            Expr2::Int(v) => Some(*v),
            Expr2::Float(f) => Some(*f as i64),
            Expr2::Date(d) => Some(*d as i64),
            _ => None,
        }
    }
}

/// Structural equality for Expr2 (for matching HAVING/ORDER BY to SELECT items).
fn exprs_equal(a: &Expr2, b: &Expr2) -> bool {
    match (a, b) {
        (Expr2::Col(x), Expr2::Col(y)) => x.eq_ignore_ascii_case(y),
        (Expr2::Int(x), Expr2::Int(y)) => x == y,
        (Expr2::Float(x), Expr2::Float(y)) => x == y,
        (Expr2::Str(x), Expr2::Str(y)) => x == y,
        (Expr2::Date(x), Expr2::Date(y)) => x == y,
        (Expr2::CountStar, Expr2::CountStar) => true,
        (Expr2::BinOp { op: o1, left: l1, right: r1 }, Expr2::BinOp { op: o2, left: l2, right: r2 }) => {
            o1 == o2 && exprs_equal(l1, l2) && exprs_equal(r1, r2)
        }
        (
            Expr2::Agg { func: f1, arg: a1, distinct: d1 },
            Expr2::Agg { func: f2, arg: a2, distinct: d2 },
        ) => f1 == f2 && d1 == d2 && exprs_equal(a1, a2),
        _ => false,
    }
}
