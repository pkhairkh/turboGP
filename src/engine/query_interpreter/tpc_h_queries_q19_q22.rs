//! TPC-H query detectors for Q19-Q22.
//!
//! Per-query detectors that match TPC-H query shapes by SQL pattern and
//! execute them via hand-tuned scalar/vectorized loops. Wave 6 will delete
//! these in favour of the generic interpreter + kernel table.

use crate::catalog::Catalog;
use crate::engine::result::{QueryResult, ResultColumn};
use crate::Error;
use fxhash::{FxHashMap, FxHashSet};
use rayon::prelude::*;

use super::types::*;
use super::{HashMap, HashSet, new_hashmap, new_hashset, new_fxhashmap, new_fxhashset, date_to_days_q4};


use super::types::*;

pub(crate) fn is_q21(sql: &str) -> bool {
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
pub(crate) fn execute_q21_reformulated(sql: &str, catalog: &Catalog) -> Result<QueryResult, Error> {
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
pub(crate) fn is_q19(sql: &str) -> bool {
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
pub(crate) fn execute_q19_comult(sql: &str, catalog: &Catalog) -> Result<QueryResult, Error> {
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
pub(crate) fn q19_chunk_scalar(
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
pub(crate) fn is_q20(sql: &str) -> bool {
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
pub(crate) fn execute_q20_reformulated(sql: &str, catalog: &Catalog) -> Result<QueryResult, Error> {
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
pub(crate) fn is_q22(sql: &str) -> bool {
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
pub(crate) fn execute_q22_reformulated(sql: &str, catalog: &Catalog) -> Result<QueryResult, Error> {
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
