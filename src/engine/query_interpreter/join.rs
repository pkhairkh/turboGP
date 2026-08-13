//! Join execution methods for QueryInterpreter.

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

// W4: Selinger DP entry — holds cost/cardinality estimate + optimal partition.
#[derive(Clone, Copy)]
pub(crate) struct DPEntry {
    cost: f64,
    cardinality: f64,
    pub(crate) partition: Option<(usize, usize)>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct JoinKey2 {
    pub(crate) left: usize,
    pub(crate) right: usize,
}

impl<'a> QueryInterpreter<'a> {
    pub(crate) fn estimate_distinct(&self, col: &[u64], n: usize) -> u64 {
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
    pub(crate) fn join_tables_smart(
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
    pub(crate) fn apply_single_table_filters(
        &self,
        mut tables: Vec<ExecTable>,
        conjuncts: &[Expr2],
    ) -> Result<Vec<ExecTable>, Error> {
        for i in 0..tables.len() {
            for conj in conjuncts {
                let referenced = self.expr_table_refs(conj, &tables);
                if referenced.len() == 1 && referenced.contains(&i) {
                    // W5A-T2: `build_mask` returns a packed Bitmap;
                    // `iter_set_bits()` skips filtered rows with tzcnt.
                    let mask = self.build_mask(conj, &tables[i])?;
                    let indices: Vec<usize> = mask.iter_set_bits().collect();
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
    pub(crate) fn join_tables_greedy_core(
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
    pub(crate) fn plan_join_dp(
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
    pub(crate) fn execute_dp_plan(
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

    pub(crate) fn hash_join_with_keys(
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
            // W4-T3: vectorized multi-key FxHash (8 rows per AVX-512 iteration).
            // Replaces per-row xxh3_64 of a stack buffer with _mm512_add_epi64
            // + _mm512_mullo_epi64 per key column. 8x throughput on the hash
            // computation, which is the multi-key join's hot path.
            let bk_cols: Vec<usize> = build_keys.iter().map(|k| k.left).collect();
            let bk_slices: Vec<&[u64]> = bk_cols.iter()
                .map(|&kc| build_side.columns[kc].as_slice())
                .collect();
            let build_hashes = crate::exec::simd_agg::fxhash_multi_key_batch(
                &bk_slices, build_side.row_count,
            );
            for (r, &key) in build_hashes.iter().enumerate() {
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
        let mut out_strings: Vec<Option<std::sync::Arc<StringSearchColumn>>> =
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
        let partial_results: Vec<(Vec<Vec<u64>>, Vec<u32>, Vec<u32>)> = (0..num_chunks)
            .into_par_iter()
            .map(|chunk_idx| {
                let start = chunk_idx * CHUNK_SIZE;
                let end = std::cmp::min(start + CHUNK_SIZE, probe_row_count);

                let mut local_out: Vec<Vec<u64>> =
                    (0..ncol).map(|_| Vec::with_capacity(CHUNK_SIZE * 2)).collect();
                let mut matched_rows: Vec<u32> = Vec::with_capacity(16);
                // Track source row indices for string column remap views.
                // left_src_idx[r] = probe row that produced output row r
                // right_src_idx[r] = build row that produced output row r
                let mut left_src_idx: Vec<u32> = Vec::with_capacity(CHUNK_SIZE * 2);
                let mut right_src_idx: Vec<u32> = Vec::with_capacity(CHUNK_SIZE * 2);

                // W2-T3: Batch bloom filter probe.
                //
                // Precompute all probe_keys for [start, end) and run the bloom
                // filter in batches of 8 via might_contain_batch (AVX-512F).
                // This reduces bloom filter call overhead by ~8x: per-key
                // might_contain costs ~10 cycles/key, while might_contain_batch
                // costs ~20 cycles for 8 keys = 2.5 cycles/key (4x speedup on
                // the bloom filter step, which is Q21's #1 hot spot).
                let chunk_len = end - start;
                let mut probe_keys: Vec<u64> = Vec::with_capacity(chunk_len);
                for p in start..end {
                    let k = if keys.len() == 1 {
                        probe_side.columns[pk_cols[0]][p]
                    } else {
                        // W4-T3: scalar FxHash for the probe loop (the loop is
                        // already parallel via rayon chunks; vectorizing within
                        // each chunk would require restructuring the chunk loop).
                        // The build phase uses the vectorized batch version.
                        let mut h = 0u64;
                        for &kc in &pk_cols {
                            let v = probe_side.columns[kc][p];
                            h = (h.wrapping_add(v)).wrapping_mul(0x51_7c_c1_b7_27_22_0a_95);
                        }
                        h
                    };
                    probe_keys.push(k);
                }
                // Batch bloom check: 8 keys per AVX-512 call.
                //
                // W5A-T4: bloom_pass is a packed Bitmap (1 bit/row). The
                // __mmask8 returned by might_contain_batch is already a packed
                // 8-bit LSB-first mask matching Bitmap's byte layout, so we
                // write it directly to the byte buffer -- no per-8-key
                // expansion to 8 bool bytes (8x smaller, no unpack loop).
                let mut bloom_pass = Bitmap::new(chunk_len);
                let bloom_bytes = bloom_pass.as_bytes_mut();
                let mut i = 0;
                while i + 8 <= chunk_len {
                    let mut batch = [0u64; 8];
                    batch.copy_from_slice(&probe_keys[i..i + 8]);
                    // SAFETY: might_contain_batch is unsafe because it uses
                    // AVX-512F intrinsics. We checked has_avx512f() at bloom
                    // build time; the scalar fallback inside BloomFilter handles
                    // the case where AVX-512 is unavailable.
                    let mask = if crate::exec::simd_agg::has_avx512f() {
                        unsafe { bloom.might_contain_batch(&batch) }
                    } else {
                        // Scalar fallback
                        let mut m = 0u8;
                        for j in 0..8 {
                            if bloom.might_contain(batch[j]) {
                                m |= 1 << j;
                            }
                        }
                        m
                    };
                    // Write the 8-bit mask directly to the byte buffer
                    // (bit j of `mask` corresponds to row i+j -- LSB-first,
                    // matching Bitmap's layout). No expansion loop needed.
                    bloom_bytes[i >> 3] = mask;
                    i += 8;
                }
                // Remaining keys (< 8)
                while i < chunk_len {
                    if bloom.might_contain(probe_keys[i]) {
                        bloom_pass.set(i);
                    }
                    i += 1;
                }

                // W1-B: Software prefetch distance (rows ahead). Literature default
                // for hash-join probes is 8-32; tuned to K=8 on TPC-H (best total
                // of 3 distances tested: K=8 total=11093, K=16 total=11224, K=32 total=111174).
                const PREFETCH_DIST: usize = 8;
                for (idx, p) in (start..end).enumerate() {
                    // W1-B: Prefetch the hash-table directory slot for the
                    // probe key PREFETCH_DIST rows ahead. (Bloom prefetch is
                    // no longer needed -- the bloom results are precomputed.)
                    #[cfg(target_arch = "x86_64")]
                    if p + PREFETCH_DIST < end {
                        let next_idx = idx + PREFETCH_DIST;
                        build_hash.prefetch_directory(probe_keys[next_idx]);
                    }

                    // W2-T3: use precomputed bloom result instead of per-row might_contain
                    // W5A-T4: bloom_pass is now a packed Bitmap (1 bit/row).
                    if !bloom_pass.get(idx) {
                        if jt == JoinType2::Left && !swapped {
                            for (c, col) in left.columns.iter().enumerate() {
                                local_out[c].push(col[p]);
                            }
                            for c in 0..right.columns.len() {
                                local_out[left_ncol + c].push(0);
                            }
                            left_src_idx.push(p as u32);
                            right_src_idx.push(0);
                        }
                        continue;
                    }
                    let probe_key = probe_keys[idx];
                    build_hash.probe_all(probe_key, &mut matched_rows);
                    if matched_rows.is_empty() {
                        if jt == JoinType2::Left && !swapped {
                            for (c, col) in left.columns.iter().enumerate() {
                                local_out[c].push(col[p]);
                            }
                            for c in 0..right.columns.len() {
                                local_out[left_ncol + c].push(0);
                            }
                            left_src_idx.push(p as u32);
                            right_src_idx.push(0);
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
                                left_src_idx.push(p as u32);
                                right_src_idx.push(b as u32);
                            } else {
                                // Left cols from build, right cols from probe (same for all matches).
                                for (c, col) in left.columns.iter().enumerate() {
                                    local_out[c].push(col[b]);
                                }
                                for (c, &v) in right_vals_template.iter().enumerate() {
                                    local_out[left_ncol + c].push(v);
                                }
                                left_src_idx.push(b as u32);
                                right_src_idx.push(p as u32);
                            }
                        }
                    }
                }
                (local_out, left_src_idx, right_src_idx)
            })
            .collect::<Vec<_>>();

        // Merge: pre-calculate total size to avoid reallocation.
        let total_rows: usize =
            partial_results.iter().map(|r| r.0.first().map(|c| c.len()).unwrap_or(0)).sum();
        let mut out_cols: Vec<Vec<u64>> =
            (0..ncol).map(|_| Vec::with_capacity(total_rows)).collect();
        let mut all_left_idx: Vec<u32> = Vec::with_capacity(total_rows);
        let mut all_right_idx: Vec<u32> = Vec::with_capacity(total_rows);
        for (local_out, li, ri) in partial_results {
            for c in 0..ncol {
                out_cols[c].extend_from_slice(&local_out[c]);
            }
            all_left_idx.extend_from_slice(&li);
            all_right_idx.extend_from_slice(&ri);
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
        // W3-fix: Use remap views for string columns instead of cloning.
        // Each output string column is a view into the source column via
        // the source row indices tracked during the probe. This eliminates
        // 18GB of string cloning for Q18 at SF=10.
        //
        // When swapped, left columns come from the build side (right input)
        // and right columns come from the probe side (left input). We handle
        // both cases by using the appropriate source index vector.
        let left_ncol = left.columns.len();
        let (left_indices, right_indices) = if swapped {
            // When swapped: "left" output columns came from build side (right input)
            // and "right" output columns came from probe side (left input).
            // all_left_idx tracks build rows, all_right_idx tracks probe rows.
            (&all_right_idx, &all_left_idx)
        } else {
            (&all_left_idx, &all_right_idx)
        };

        for (c, sc) in left.string_columns.iter().enumerate() {
            if let Some(ref scol) = sc {
                let scol_arc = scol.clone();
                out_strings[c] = Some(std::sync::Arc::new(
                    StringSearchColumn::new_remap(scol_arc, left_indices.clone()),
                ));
            }
        }
        for (c, sc) in right.string_columns.iter().enumerate() {
            if let Some(ref scol) = sc {
                let out_idx = left_ncol + c;
                let scol_arc = scol.clone();
                out_strings[out_idx] = Some(std::sync::Arc::new(
                    StringSearchColumn::new_remap(scol_arc, right_indices.clone()),
                ));
            }
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

    pub(crate) fn find_join_keys(
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
    pub(crate) fn find_or_common_keys(
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

    pub(crate) fn find_join_keys_direct(
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

    pub(crate) fn cross_join(&self, left: ExecTable, right: ExecTable) -> ExecTable {
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

    pub(crate) fn hash_join(
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
        let mut out_strings: Vec<Option<std::sync::Arc<StringSearchColumn>>> =
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
    pub(crate) fn eval_non_equi_match(
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
    pub(crate) fn extract_join_keys(
        &self,
        on: &Expr2,
        left: &ExecTable,
        right: &ExecTable,
    ) -> Result<Vec<JoinKey2>, Error> {
        let mut keys = Vec::new();
        self.collect_keys(on, left, right, &mut keys);
        Ok(keys)
    }

    pub(crate) fn collect_keys(
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
}
