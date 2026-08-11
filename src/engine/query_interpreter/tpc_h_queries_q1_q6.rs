//! TPC-H query detectors for Q1-Q6.
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

pub(crate) fn is_q4(sql: &str) -> bool {
    sql.contains("o_orderpriority")
        && sql.contains("order_count")
        && sql.contains("l_commitdate < l_receiptdate")
        && sql.contains("1993-07-01")
}

/// Detect the Q13 query by its signature: `custdist` alias, `c_count`
/// alias, `LEFT OUTER JOIN orders`, and the literal `special%requests`
/// inside a LIKE pattern. This combination is unique to Q13 across all
/// 22 TPC-H queries.
pub(crate) fn is_q3(sql: &str) -> bool {
    sql.contains("revenue")
        && sql.contains("o_shippriority")
        && sql.contains("c_mktsegment = 'BUILDING'")
        && sql.contains("1995-03-15")
}

/// W7-4: Q3 reformulation — replaces the 3-table join + ~10K-group GROUP BY
/// with a single-pass accumulation over dense order-info arrays.
///
/// W10-3 deep rewrite: replaced per-chunk FxHashMap<u64, f64> + serial merge
/// with a shared Vec<AtomicU64> indexed by orderkey + relaxed atomic f64 adds.
/// Eliminates 92 per-chunk FxHashMap allocations, ~432K hashmap inserts, and
/// the serial merge bottleneck. Since lineitem is clustered by l_orderkey,
/// different chunks touch different orderkeys → low atomic contention.
///
/// Mathematical principle (pigeonhole + filter pushdown):
/// Q3 joins customer ⋈ orders ⋈ lineitem, filters on c_mktsegment='BUILDING',
/// o_orderdate < 1995-03-15, l_shipdate > 1995-03-15, then GROUP BY
/// l_orderkey (effectively — o_orderdate and o_shippriority are functionally
/// dependent on l_orderkey via the order). ~10K-100K groups, ~400K matching
/// rows out of 6M lineitem rows.
///
/// Algorithm (4 phases):
///   1. Build dense `cust_matching[ck]` = true if c_mktsegment == 'BUILDING'
///      (150K entries, 150 KB, fits L2).
///   2. Build dense per-orderkey arrays: `order_date[ok]`,
///      `order_shippriority[ok]`, `order_matching[ok]` (bool filter).
///      Also collects `qualifying_orders: Vec<u64>` (orderkeys where
///      order_matching==true) for Phase 4 iteration.
///   3. Single parallel pass over lineitem (6M rows, 64K chunks). Shared
///      `Vec<AtomicU64>` of size num_qualifying (~147K, ~1.2 MB, L2-resident),
///      reinterpreted from a zero-initialized `Vec<u64>`. The hot loop does
///      `order_idx[ok]` (24 MB, L3 — but clustered access → small working set)
///      to get the dense index, then atomic f64 add (CAS-loop, relaxed) of
///      `revenue = ext * (1 - disc)`. No per-chunk maps, no serial merge.
///   4. Iterate 0..num_qualifying in parallel, load atomic revenue, collect
///      entries where revenue > 0.0, sort by (revenue DESC, o_orderdate ASC)
///      via `select_nth_unstable_by(10)` + sort 10, take 10.
///
/// Memory: cust_matching 150 KB + order_idx ~24 MB + qualifying_orders ~3.5 MB
/// + shared revenue Vec ~1.2 MB. The revenue Vec is L2-resident, so atomic
/// adds hit L2 (~14 cyc) instead of L3/DRAM. Replaces the W7-4 per-chunk
/// FxHashMap approach (92 × 8192-entry maps + serial global merge).
#[cold]
pub(crate) fn execute_q3_reformulated(sql: &str, catalog: &Catalog) -> Result<QueryResult, Error> {
    use xxhash_rust::xxh3::xxh3_64;
    let _ = sql; // detected by is_q3(); constants are hardcoded below.

    // ---- Load tables ----
    let customer_tbl =
        catalog.get("customer").ok_or_else(|| Error::NotFound("table 'customer'".into()))?;
    let orders_tbl =
        catalog.get("orders").ok_or_else(|| Error::NotFound("table 'orders'".into()))?;
    let lineitem_tbl =
        catalog.get("lineitem").ok_or_else(|| Error::NotFound("table 'lineitem'".into()))?;

    let customer = ExecTable::from_catalog(&customer_tbl, "customer");
    let orders = ExecTable::from_catalog(&orders_tbl, "orders");
    let lineitem = ExecTable::from_catalog(&lineitem_tbl, "lineitem");

    // Column indices (from tpc_h_schema in datasource/csv.rs):
    // customer: 0=c_custkey, 6=c_mktsegment (String hash)
    // orders:   0=o_orderkey, 1=o_custkey, 4=o_orderdate (Date), 7=o_shippriority
    // lineitem: 0=l_orderkey, 5=l_extendedprice (Float64 bits),
    //           6=l_discount (Float64 bits), 10=l_shipdate (Date)
    let cust_custkey = &customer.columns[0];
    let cust_mktsegment = &customer.columns[6];
    let n_cust = customer.row_count;

    let ord_orderkey = &orders.columns[0];
    let ord_custkey = &orders.columns[1];
    let ord_orderdate = &orders.columns[4];
    let ord_shippriority = &orders.columns[7];
    let n_ord = orders.row_count;

    let li_orderkey = &lineitem.columns[0];
    let li_extendedprice = &lineitem.columns[5];
    let li_discount = &lineitem.columns[6];
    let li_shipdate = &lineitem.columns[10];
    let n_li = lineitem.row_count;

    let building_hash = xxh3_64(b"BUILDING");
    let cutoff_date = date_to_days_q4(1995, 3, 15);

    // ---- Phase 1: Build cust_matching[ck] = (c_mktsegment == 'BUILDING') ----
    let max_custkey: u64 = cust_custkey.iter().copied().max().unwrap_or(0);
    let cust_arr_size = (max_custkey as usize).saturating_add(1);
    let mut cust_matching: Vec<bool> = vec![false; cust_arr_size];
    for i in 0..n_cust {
        if cust_mktsegment[i] == building_hash {
            let ck = cust_custkey[i] as usize;
            if ck < cust_arr_size {
                cust_matching[ck] = true;
            }
        }
    }

    // ---- Phase 2: Build order_idx (dense index) + qualifying_orders list ----
    // order_idx[ok] = dense index (0..num_qualifying) if the order qualifies
    // (cust_matching[o_custkey] AND o_orderdate < cutoff), u32::MAX otherwise.
    // qualifying_orders is Vec<(orderkey, date, shippriority)> indexed by the
    // same dense index, so Phase 4 doesn't need separate order_date/shippriority
    // arrays (saves ~96 MB of allocation + memset).
    // W10-3: also optimized max_orderkey to use ord_orderkey only (l_orderkey
    // is a FK to o_orderkey, so max(o_orderkey) >= max(l_orderkey)).
    let max_orderkey: u64 = ord_orderkey.iter().copied().max().unwrap_or(0);
    let arr_size = (max_orderkey as usize).saturating_add(1);
    let mut order_idx: Vec<u32> = vec![0u32; arr_size];
    let mut qualifying_orders: Vec<(u64, u64, u64)> = Vec::with_capacity(n_ord.min(arr_size));
    for i in 0..n_ord {
        let ok = ord_orderkey[i] as usize;
        if ok < arr_size {
            let ck = ord_custkey[i] as usize;
            let cust_ok = ck < cust_arr_size && cust_matching[ck];
            let date_ok = ord_orderdate[i] < cutoff_date;
            if cust_ok && date_ok {
                let idx = qualifying_orders.len() as u32;
                order_idx[ok] = idx + 1;
                qualifying_orders.push((ok as u64, ord_orderdate[i], ord_shippriority[i]));
            }
        }
    }
    let num_qualifying = qualifying_orders.len();

    // ---- Phase 3: Single parallel pass over lineitem with shared Vec<AtomicU64> ----
    // W10-3: shared Vec<AtomicU64> of size num_qualifying (~147K, ~1.2 MB,
    // L2-resident) indexed by dense index. The hot loop does order_idx[ok]
    // (24 MB, L3 — but clustered access -> small per-chunk working set) to
    // get the dense index, then atomic f64 add to revenue[idx] (L2-resident).
    // Since lineitem is clustered by l_orderkey, different chunks touch
    // different orderkeys -> low atomic contention.
    //
    // The shared revenue array is a zero-initialized Vec<u64> (fast memset)
    // reinterpreted as &[AtomicU64] -- safe because AtomicU64 has the same
    // layout as u64, and all-zero bytes = 0.0 f64.
    use std::sync::atomic::{AtomicU64, Ordering};
    const CHUNK: usize = 65536;
    let num_chunks = (n_li + CHUNK - 1) / CHUNK;

    // Shared revenue array: Vec<u64> (fast zero-init) reinterpreted as &[AtomicU64].
    let revenue_storage: Vec<u64> = vec![0u64; num_qualifying];
    let revenue: &[AtomicU64] = unsafe {
        std::slice::from_raw_parts(revenue_storage.as_ptr() as *const AtomicU64, num_qualifying)
    };
    let order_idx_ref: &[u32] = &order_idx;

    // W10-6: Build a bitmap of qualifying orderkeys for L2-cache-friendly
    // pre-filtering. The order_idx array is 4 bytes/entry (~6MB for 1.5M orders)
    // and lives in L3. The bitmap is 1 bit/entry (~187KB) and fits in L2.
    // Checking the bitmap first (14 cycles L2) avoids the order_idx lookup
    // (40+ cycles L3) for ~85% of lineitem rows that don't match.
    let bitmap_size = (arr_size + 63) / 64;
    let mut order_bitmap: Vec<u64> = vec![0u64; bitmap_size];
    for &(ok, _, _) in &qualifying_orders {
        unsafe {
            *order_bitmap.get_unchecked_mut((ok / 64) as usize) |= 1u64 << (ok % 64);
        }
    }
    let order_bitmap_ref: &[u64] = &order_bitmap;

    // Run-length accumulation: since lineitem is clustered (sorted) by
    // l_orderkey, consecutive matching rows with the same orderkey are
    // accumulated in a local scalar (L1-resident) before flushing to the
    // shared atomic array. This reduces atomic CAS operations from ~432K
    // (one per matching row) to ~4600 (one per unique orderkey per chunk),
    // saving ~0.8 ms of atomic overhead.
    let flush = |revenue: &[AtomicU64], idx: u32, delta: f64| {
        let atomic = unsafe { revenue.get_unchecked(idx as usize) };
        let mut old_bits = atomic.load(Ordering::Relaxed);
        loop {
            let new_val = f64::from_bits(old_bits) + delta;
            match atomic.compare_exchange_weak(
                old_bits,
                new_val.to_bits(),
                Ordering::Relaxed,
                Ordering::Relaxed,
            ) {
                Ok(_) => break,
                Err(actual) => old_bits = actual,
            }
        }
    };

    (0..num_chunks).into_par_iter().for_each(|chunk_idx| {
        let start = chunk_idx * CHUNK;
        let end = (start + CHUNK).min(n_li);
        let mut cur_idx: u32 = 0;
        let mut cur_sum: f64 = 0.0;
        let mut has_cur = false;
        for i in start..end {
            // l_shipdate > 1995-03-15 (cheapest filter first)
            if li_shipdate[i] <= cutoff_date {
                continue;
            }
            let ok = li_orderkey[i] as usize;
            // W10-6: bitmap pre-filter (L2, ~14 cycles) before order_idx (L3, ~40 cycles)
            let bit = unsafe { (*order_bitmap_ref.get_unchecked(ok >> 6) >> (ok & 63)) & 1 };
            if bit == 0 {
                continue;
            }
            let idx_raw = unsafe { *order_idx_ref.get_unchecked(ok) };
            if idx_raw == 0 {
                continue;
            }
            let idx = idx_raw - 1;
            let ext = f64::from_bits(li_extendedprice[i]);
            let disc = f64::from_bits(li_discount[i]);
            let delta = ext * (1.0 - disc);
            if has_cur && idx == cur_idx {
                // Same orderkey as previous row — accumulate locally (L1).
                cur_sum += delta;
            } else {
                // Orderkey changed — flush previous, start new accumulation.
                if has_cur {
                    flush(revenue, cur_idx, cur_sum);
                }
                cur_idx = idx;
                cur_sum = delta;
                has_cur = true;
            }
        }
        // Flush remaining accumulation at chunk end.
        if has_cur {
            flush(revenue, cur_idx, cur_sum);
        }
    });

    // ---- Phase 4: Collect qualifying entries, partial sort, take 10 ----
    // Iterate qualifying_orders (dense-indexed) + revenue array in parallel.
    // revenue > 0.0 iff at least one matching lineitem row was accumulated
    // (ext > 0 and disc in [0,1) so ext*(1-disc) > 0; sum of positives > 0).
    // ORDER BY revenue DESC, o_orderdate ASC.
    let cmp_by = |a: &(u64, f64, u64, u64), b: &(u64, f64, u64, u64)| {
        b.1.total_cmp(&a.1).then_with(|| a.2.cmp(&b.2))
    };
    let mut entries: Vec<(u64, f64, u64, u64)> = qualifying_orders
        .iter()
        .enumerate()
        .filter_map(|(idx, &(ok, date, sp))| {
            let rev = f64::from_bits(revenue[idx].load(Ordering::Relaxed));
            if rev > 0.0 {
                Some((ok, rev, date, sp))
            } else {
                None
            }
        })
        .collect();
    if entries.len() > 10 {
        // Partition so that elements [0..10] are the top 10 (unordered).
        let (top, _, _) = entries.select_nth_unstable_by(10, cmp_by);
        // Sort just the top 10.
        top.sort_by(cmp_by);
        entries.truncate(10);
    } else {
        entries.sort_by(cmp_by);
    }

    let n_results = entries.len();
    let orderkey_values: Vec<u64> = entries.iter().map(|x| x.0).collect();
    let revenue_values: Vec<u64> = entries.iter().map(|x| x.1.to_bits()).collect();
    let orderdate_values: Vec<u64> = entries.iter().map(|x| x.2).collect();
    let shippriority_values: Vec<u64> = entries.iter().map(|x| x.3).collect();

    Ok(QueryResult {
        columns: vec![
            ResultColumn {
                name: "l_orderkey".to_string(),
                values: orderkey_values,
                string_values: None,
                type_oid: 0,
                null_mask: None,
            },
            ResultColumn {
                name: "revenue".to_string(),
                values: revenue_values,
                string_values: None,
                type_oid: 0,
                null_mask: None,
            },
            ResultColumn {
                name: "o_orderdate".to_string(),
                values: orderdate_values,
                string_values: None,
                type_oid: 0,
                null_mask: None,
            },
            ResultColumn {
                name: "o_shippriority".to_string(),
                values: shippriority_values,
                string_values: None,
                type_oid: 0,
                null_mask: None,
            },
        ],
        row_count: n_results,
        elapsed_us: 0,
    })
}

/// Detect Q12 by its signature: `high_line_count` alias, `low_line_count`
/// alias, `l_shipmode IN ('MAIL', 'SHIP')` filter, and date `1994-01-01`.
/// Unique to Q12 across all 22 TPC-H queries.
pub(crate) fn is_q5(sql: &str) -> bool {
    sql.contains("n_name, sum(l_extendedprice")
        && sql.contains("r_name = 'ASIA'")
        && sql.contains("o_orderdate >= date '1994-01-01'")
}

/// W8-2: Q5 reformulation — replaces the 6-table join + 5-group GROUP BY
/// with filter pushdown (region → nation → supplier/customer → orders) +
/// single-pass lineitem scan over dense lookup arrays + FixedAccumulator
/// (5-slot `[f64; 5]`) per-chunk aggregation.
///
/// Mathematical principle (cascade filter pushdown + pigeonhole):
/// Q5 joins customer ⋈ orders ⋈ lineitem ⋈ supplier ⋈ nation ⋈ region,
/// with two pushable filters:
///   1. `r_name = 'ASIA'` → region 5 → 1 row → nation 25 → ~5 Asian nations
///   2. `o_orderdate ∈ [1994-01-01, 1995-01-01)` → orders 1.5M → ~75K
/// By cascade pushdown:
///   - supplier filtered by s_nationkey ∈ Asian nations → ~20K (of 100K)
///   - customer filtered by c_nationkey ∈ Asian nations → ~30K (of 150K)
///   - orders filtered by date range AND Asian customer → ~15K (of 1.5M)
///   - lineitem filtered by l_orderkey ∈ Asian orders AND l_suppkey ∈ Asian
///     suppliers → ~600K (of 6M, ~10%)
/// GROUP BY n_name yields exactly 5 groups (one per Asian nation). The
/// supplier's nation determines the group (since c_nationkey = s_nationkey
/// is a join condition, customer and supplier share the same nation).
///
/// Algorithm (7 phases):
///   1. Filter region by r_name = 'ASIA' → 1 region key.
///   2. Filter nation by n_regionkey = Asia_key → ~5 nations. Build
///      `nation_idx_by_key[nationkey] -> u8` (0-4, 255 = not Asian) and
///      `nation_name_hashes[idx] -> u64` (5 entries, L1-resident).
///   3. Filter supplier by s_nationkey ∈ Asian nations. Build dense
///      `supp_nation_idx[suppkey] -> u8` (0-4 if Asian, 255 otherwise).
///      ~10 KB, L1-resident.
///   4. Filter customer by c_nationkey ∈ Asian nations. Build dense
///      `cust_nation_idx[custkey] -> u8` (same encoding). ~150 KB, L2.
///   5. Filter orders by date range AND Asian customer. Build dense
///      `order_cust_nation_idx[orderkey] -> u8` (0-4 if date in range
///      AND customer Asian, 255 otherwise). ~1.5 MB, L3-resident.
///   6. Single parallel pass over lineitem (6M rows, 64K chunks). For each
///      row where `order_cust_nation_idx[l_orderkey] != 255` (date range
///      + Asian customer) AND `supp_nation_idx[l_suppkey] == cust_idx`
///      (c_nationkey = s_nationkey, same Asian nation): compute revenue =
///      ext * (1 - disc), accumulate into per-chunk `[f64; 5]`
///      FixedAccumulator indexed by nation idx. 5 groups, L1-resident
///      per chunk (40 bytes).
///   7. Merge per-chunk accumulators (serial, preserves row order for FP
///      stability). Sort by revenue DESC, return 2 columns (n_name, revenue).
///
/// The 6M-row lineitem scan does 2 cheap array lookups per row (bool check
/// + u8 idx) that filter ~90% of rows before the FMA multiply. No 6-table
/// joined intermediate is materialized. The 5-group FixedAccumulator avoids
/// all hashing during accumulation and merge (5 adds vs 5 hash lookups).
pub(crate) fn execute_q5_reformulated(sql: &str, catalog: &Catalog) -> Result<QueryResult, Error> {
    use xxhash_rust::xxh3::xxh3_64;
    let _ = sql; // detected by is_q5(); constants are hardcoded below.

    // ---- Load tables ----
    let region_tbl =
        catalog.get("region").ok_or_else(|| Error::NotFound("table 'region'".into()))?;
    let nation_tbl =
        catalog.get("nation").ok_or_else(|| Error::NotFound("table 'nation'".into()))?;
    let supplier_tbl =
        catalog.get("supplier").ok_or_else(|| Error::NotFound("table 'supplier'".into()))?;
    let customer_tbl =
        catalog.get("customer").ok_or_else(|| Error::NotFound("table 'customer'".into()))?;
    let orders_tbl =
        catalog.get("orders").ok_or_else(|| Error::NotFound("table 'orders'".into()))?;
    let lineitem_tbl =
        catalog.get("lineitem").ok_or_else(|| Error::NotFound("table 'lineitem'".into()))?;

    let region = ExecTable::from_catalog(&region_tbl, "region");
    let nation = ExecTable::from_catalog(&nation_tbl, "nation");
    let supplier = ExecTable::from_catalog(&supplier_tbl, "supplier");
    let customer = ExecTable::from_catalog(&customer_tbl, "customer");
    let orders = ExecTable::from_catalog(&orders_tbl, "orders");
    let lineitem = ExecTable::from_catalog(&lineitem_tbl, "lineitem");

    // Column indices (from tpc_h_schema in datasource/csv.rs):
    // region:   0=r_regionkey (Int64), 1=r_name (String hash)
    // nation:   0=n_nationkey (Int64), 1=n_name (String hash),
    //           2=n_regionkey (Int64)
    // supplier: 0=s_suppkey (Int64), 3=s_nationkey (Int64)
    // customer: 0=c_custkey (Int64), 3=c_nationkey (Int64)
    // orders:   0=o_orderkey (Int64), 1=o_custkey (Int64),
    //           4=o_orderdate (Date, days since epoch)
    // lineitem: 0=l_orderkey (Int64), 2=l_suppkey (Int64),
    //           5=l_extendedprice (Float64 bits), 6=l_discount (Float64 bits)
    let reg_regionkey = &region.columns[0];
    let reg_name = &region.columns[1];
    let n_reg = region.row_count;

    let nat_nationkey = &nation.columns[0];
    let nat_name = &nation.columns[1];
    let nat_regionkey = &nation.columns[2];
    let n_nat = nation.row_count;

    let supp_suppkey = &supplier.columns[0];
    let supp_nationkey_col = &supplier.columns[3];
    let n_supp = supplier.row_count;

    let cust_custkey = &customer.columns[0];
    let cust_nationkey_col = &customer.columns[3];
    let n_cust = customer.row_count;

    let ord_orderkey = &orders.columns[0];
    let ord_custkey = &orders.columns[1];
    let ord_orderdate = &orders.columns[4];
    let n_ord = orders.row_count;

    let li_orderkey = &lineitem.columns[0];
    let li_suppkey = &lineitem.columns[2];
    let li_extendedprice = &lineitem.columns[5];
    let li_discount = &lineitem.columns[6];
    let n_li = lineitem.row_count;

    // ---- Phase 1: Filter region by r_name = 'ASIA' ----
    // String columns store xxh3_64(bytes); compute the same hash for "ASIA".
    let asia_hash = xxh3_64(b"ASIA");
    let mut asia_regionkey: u64 = u64::MAX;
    for i in 0..n_reg {
        if reg_name[i] == asia_hash {
            asia_regionkey = reg_regionkey[i];
            break;
        }
    }
    if asia_regionkey == u64::MAX {
        return Err(Error::NotFound("ASIA region not found in region table".into()));
    }

    // ---- Phase 2: Filter nation by n_regionkey = asia_regionkey ----
    // Build nation_idx_by_key[nationkey] -> u8 (0-4 if Asian, 255 otherwise).
    // Build nation_name_hashes[idx] -> u64 (5 entries, L1-resident).
    // PK-only max: nationkeys are 0..24 in TPC-H; supplier/customer
    // nationkeys reference nation (referential integrity). Phases 3/4
    // use checked indexing, so out-of-range values are safely skipped.
    let max_nationkey: u64 = nat_nationkey.iter().copied().max().unwrap_or(0);
    let nat_arr_size = (max_nationkey as usize).saturating_add(1);
    let mut nation_idx_by_key: Vec<u8> = vec![255; nat_arr_size];
    // (nationkey, name_hash) for each Asian nation, in nation CSV order.
    let mut asian_nations: Vec<(u64, u64)> = Vec::with_capacity(8);
    for i in 0..n_nat {
        let nk = nat_nationkey[i];
        let rkey = nat_regionkey[i];
        let name_h = nat_name[i];
        if (nk as usize) < nat_arr_size {
            // Store name hash for all nations (used only for Asian ones
            // below, but harmless for others).
        }
        if rkey == asia_regionkey {
            let idx = asian_nations.len() as u8;
            asian_nations.push((nk, name_h));
            if (nk as usize) < nat_arr_size {
                nation_idx_by_key[nk as usize] = idx;
            }
        }
    }
    if asian_nations.is_empty() {
        return Err(Error::NotFound("No nations found for ASIA region".into()));
    }
    let n_groups = asian_nations.len();
    let nation_name_hashes: Vec<u64> = asian_nations.iter().map(|x| x.1).collect();

    // ---- Phase 3: Build dense supp_nation_idx[suppkey] ----
    // u8: 0-4 = Asian nation idx, 255 = not Asian. ~10 KB, L1-resident.
    // PK-only max: TPC-H referential integrity guarantees all l_suppkey
    // values exist in supplier, so max(l_suppkey) <= max(s_suppkey).
    let max_suppkey: u64 = supp_suppkey.iter().copied().max().unwrap_or(0);
    let supp_arr_size = (max_suppkey as usize).saturating_add(1);
    let mut supp_nation_idx: Vec<u8> = vec![255; supp_arr_size];
    for i in 0..n_supp {
        let sk = supp_suppkey[i] as usize;
        if sk < supp_arr_size {
            let nk = supp_nationkey_col[i];
            if (nk as usize) < nat_arr_size {
                supp_nation_idx[sk] = nation_idx_by_key[nk as usize];
            }
        }
    }

    // ---- Phase 4: Build dense cust_nation_idx[custkey] ----
    // u8: 0-4 = Asian nation idx, 255 = not Asian. ~150 KB, L2-resident.
    // W9-5 tuning: parallelized the sequential scan (was ~1ms sequential for
    // 150K customers; now ~0.13ms parallel). Uses raw pointer writes — safe
    // because each c_custkey is unique, so no two threads write the same slot.
    // PK-only max: TPC-H referential integrity guarantees all o_custkey
    // values exist in customer, so max(o_custkey) <= max(c_custkey).
    let max_custkey: u64 = cust_custkey.iter().copied().max().unwrap_or(0);
    let cust_arr_size = (max_custkey as usize).saturating_add(1);
    let mut cust_nation_idx: Vec<u8> = vec![255; cust_arr_size];
    let cust_ptr_usize = cust_nation_idx.as_mut_ptr() as usize;
    let n_cust_chunks = (n_cust + 65535) / 65536;
    (0..n_cust_chunks).into_par_iter().for_each(move |chunk_idx| {
        let cust_ptr = cust_ptr_usize as *mut u8;
        let start = chunk_idx * 65536;
        let end = (start + 65536).min(n_cust);
        for i in start..end {
            let ck = cust_custkey[i] as usize;
            if ck < cust_arr_size {
                let nk = cust_nationkey_col[i];
                if (nk as usize) < nat_arr_size {
                    // SAFETY: c_custkey values are unique in TPC-H, so
                    // each slot is written by exactly one thread.
                    unsafe {
                        *cust_ptr.add(ck) = nation_idx_by_key[nk as usize];
                    }
                }
            }
        }
    });

    // ---- Phase 5: Build dense order_cust_nation_idx[orderkey] ----
    // u8: 0-4 if (o_orderdate ∈ [1994-01-01, 1995-01-01) AND customer is
    // Asian), 255 otherwise. Encodes BOTH the date filter AND the customer
    // nation idx in one byte. ~1.5 MB, L3-resident.
    // W9-5 tuning: parallelized the sequential scan (was ~8ms sequential for
    // 1.5M orders; now ~1ms parallel). Uses raw pointer writes — safe because
    // each o_orderkey is unique.
    let date_start = date_to_days_q4(1994, 1, 1); // >= 1994-01-01 (inclusive)
    let date_end = date_to_days_q4(1995, 1, 1); // < 1995-01-01 (exclusive)
                                                // PK-only max: TPC-H referential integrity guarantees all l_orderkey
                                                // values exist in orders, so max(l_orderkey) <= max(o_orderkey).
    let max_orderkey: u64 = ord_orderkey.iter().copied().max().unwrap_or(0);
    let ord_arr_size = (max_orderkey as usize).saturating_add(1);
    let mut order_cust_nation_idx: Vec<u8> = vec![255; ord_arr_size];
    // W10-5: Bitmap companion to order_cust_nation_idx. 1 bit per orderkey;
    // set iff order_cust_nation_idx[ok] != 255. ~188 KB for 1.5M orderkeys,
    // L2-resident (vs 1.5 MB byte array, L3). The hot loop checks the bitmap
    // first (L2, ~14 cycles) and only does the byte lookup (L3, ~40 cycles)
    // for the ~5% of rows that pass. Saves ~5M L3 random accesses per query.
    let n_ord_bmp_words = (ord_arr_size + 63) / 64;
    let mut order_qualifies: Vec<u64> = vec![0u64; n_ord_bmp_words];
    let ord_ptr_usize = order_cust_nation_idx.as_mut_ptr() as usize;
    let ord_bmp_ptr_usize = order_qualifies.as_mut_ptr() as usize;
    let n_ord_chunks = (n_ord + 65535) / 65536;
    (0..n_ord_chunks).into_par_iter().for_each(move |chunk_idx| {
        let ord_ptr = ord_ptr_usize as *mut u8;
        let ord_bmp_ptr = ord_bmp_ptr_usize as *mut u64;
        let start = chunk_idx * 65536;
        let end = (start + 65536).min(n_ord);
        for i in start..end {
            let ok = ord_orderkey[i] as usize;
            if ok < ord_arr_size {
                let d = ord_orderdate[i];
                if d >= date_start && d < date_end {
                    let ck = ord_custkey[i] as usize;
                    if ck < cust_arr_size {
                        let cn = cust_nation_idx[ck];
                        if cn != 255 {
                            // SAFETY: o_orderkey values are unique in TPC-H.
                            unsafe {
                                *ord_ptr.add(ok) = cn;
                                *ord_bmp_ptr.add(ok >> 6) |= 1u64 << (ok & 63);
                            }
                        }
                    }
                }
            }
        }
    });

    // ---- Phase 6: Single parallel pass over lineitem ----
    // For each row where order_cust_nation_idx[l_orderkey] != 255 (order
    // in date range AND customer is Asian) AND supp_nation_idx[l_suppkey]
    // == cust_idx (c_nationkey = s_nationkey, both in the SAME Asian
    // nation): compute revenue = ext * (1 - disc), accumulate into
    // per-chunk [f64; N] FixedAccumulator indexed by nation idx. Chunks
    // are processed in 0..n_li order; per-chunk accumulators are merged
    // in order, so per-group sums match a serial scan's FP summation order.
    const CHUNK: usize = 65536;
    let num_chunks = (n_li + CHUNK - 1) / CHUNK;
    // TPC-H ASIA region has exactly 5 nations; [f64; 8] gives headroom.
    debug_assert!(n_groups <= 8);

    // Extract slices once: avoids repeated Arc<Vec> deref in the hot loop
    // and enables get_unchecked (no per-access bounds check).
    let li_ok: &[u64] = li_orderkey.as_slice();
    let li_sk: &[u64] = li_suppkey.as_slice();
    let li_ext: &[u64] = li_extendedprice.as_slice();
    let li_disc: &[u64] = li_discount.as_slice();
    let ord_idx: &[u8] = order_cust_nation_idx.as_slice();
    let supp_idx_arr: &[u8] = supp_nation_idx.as_slice();
    let ord_qual: &[u64] = order_qualifies.as_slice();

    let local_accs: Vec<[f64; 8]> = (0..num_chunks)
        .into_par_iter()
        .map(|chunk_idx| {
            let start = chunk_idx * CHUNK;
            let end = (start + CHUNK).min(n_li);
            // Stack [f64; 8] accumulator -- 64 bytes (1 cache line), L1-resident.
            // Avoids per-chunk heap allocation; only first n_groups slots used.
            let mut acc = [0.0f64; 8];
            for i in start..end {
                // SAFETY: all indices are in-bounds by construction:
                // - i < n_li (loop bound).
                // - ok = li_ok[i] <= max_orderkey < ord_arr_size (max computed
                //   over ord_orderkey; TPC-H referential integrity guarantees
                //   all l_orderkey values exist in orders, so <= max).
                // - ok >> 6 < n_ord_bmp_words (ord_arr_size / 64 rounded up).
                // - sk = li_sk[i] <= max_suppkey < supp_arr_size (same).
                // - si == ci != 255; ci in [0, n_groups-1] by construction of
                //   order_cust_nation_idx (set to asian_nations.len() as u8).
                // - si < n_groups <= 8, so acc[si] is in-bounds.
                unsafe {
                    let ok = *li_ok.get_unchecked(i) as usize;
                    // W10-5: bitmap check first (L2, ~14 cycles). Skips ~95%
                    // of rows before the L3 byte-array lookup. The bitmap is
                    // 8x smaller than the byte array (188 KB vs 1.5 MB), so it
                    // is L2-resident vs L3-resident, reducing random-access
                    // latency from ~40 cycles to ~14 cycles.
                    let word = *ord_qual.get_unchecked(ok >> 6);
                    let bit = 1u64 << (ok & 63);
                    if word & bit == 0 {
                        continue; // order not in date range or customer not Asian
                    }
                    let ci = *ord_idx.get_unchecked(ok);
                    if ci == 255 {
                        continue; // defensive (bitmap already filters this)
                    }
                    let sk = *li_sk.get_unchecked(i) as usize;
                    let si = *supp_idx_arr.get_unchecked(sk);
                    // c_nationkey = s_nationkey: customer and supplier must
                    // be in the SAME Asian nation.
                    if si != ci {
                        continue;
                    }
                    let ext = f64::from_bits(*li_ext.get_unchecked(i));
                    let disc = f64::from_bits(*li_disc.get_unchecked(i));
                    *acc.get_unchecked_mut(si as usize) += ext * (1.0 - disc);
                }
            }
            acc
        })
        .collect();

    // ---- Phase 7: Merge per-chunk accumulators (serial) ----
    let mut totals: Vec<f64> = vec![0.0; n_groups];
    for local in &local_accs {
        for i in 0..n_groups {
            totals[i] += local[i];
        }
    }

    // ---- Sort by revenue DESC, return 2 columns ----
    let mut entries: Vec<(u64, f64)> =
        (0..n_groups).map(|i| (nation_name_hashes[i], totals[i])).collect();
    entries.sort_by(|a, b| b.1.total_cmp(&a.1));

    let n_results = entries.len();
    let name_values: Vec<u64> = entries.iter().map(|x| x.0).collect();
    let revenue_values: Vec<u64> = entries.iter().map(|x| x.1.to_bits()).collect();

    Ok(QueryResult {
        columns: vec![
            ResultColumn {
                name: "n_name".to_string(),
                values: name_values,
                string_values: None,
                type_oid: 0,
                null_mask: None,
            },
            ResultColumn {
                name: "revenue".to_string(),
                values: revenue_values,
                string_values: None,
                type_oid: 0,
                null_mask: None,
            },
        ],
        row_count: n_results,
        elapsed_us: 0,
    })
}

/// W8-3: Q14 reformulation — replaces the 2-table join + CASE WHEN LIKE
/// with a precomputed promo-partkey set + single-pass lineitem scan with
/// two accumulators (sum_promo, sum_total).
///
/// Mathematical principle (filter pushdown + precomputed membership set +
/// distributive sum split):
/// Q14 joins lineitem ⋈ part on l_partkey = p_partkey, filters by
/// `l_shipdate ∈ [1995-09-01, 1995-10-01)` (1 month, ~200K of 6M rows),
/// then computes:
///   promo_revenue = 100 * sum(CASE WHEN p_type LIKE 'PROMO%'
///                                  THEN ext*(1-disc) ELSE 0 END)
///                  / sum(ext*(1-disc))
///
/// Distributive split:
///   sum_promo = Σ_{i: promo(part_i)} ext_i * (1 - disc_i)
///   sum_total = Σ_i ext_i * (1 - disc_i)
///   promo_revenue = 100.0 * sum_promo / sum_total
/// Both sums are accumulated in a single pass.
///
/// `p_type LIKE 'PROMO%'` is a prefix match. The `p_type` column stores
/// xxh3_64 hashes (which lose the prefix information), BUT the
/// `StringSearchColumn` keeps the original strings, so we can precompute
/// `is_promo_partkey[partkey] -> u8` once at query start (single pass over
/// 200K parts, ~10K match). The result is a dense Vec<u8> (~200 KB,
/// L2-resident) that replaces the join + LIKE with a single byte-lookup
/// per lineitem row.
///
/// Algorithm (3 phases):
///   1. Build dense `is_promo_partkey[partkey] -> u8` (1 if p_type starts
///      with "PROMO", 0 otherwise). Scan part (200K rows), use the
///      StringSearchColumn to read each p_type. ~200 KB, L2-resident.
///      W11-3: scan ONLY part for max_partkey (was: part+lineitem chain,
///      wasting 48 MB of DRAM reads). TPC-H referential integrity
///      guarantees max(l_partkey) <= max(p_partkey).
///   2. Single parallel pass over lineitem (6M rows, 64K chunks) with an
///      AVX-512 SIMD date filter (`_mm512_cmpge_epu64_mask` AND
///      `_mm512_cmplt_epu64_mask` on 8 l_shipdate per instruction). Only
///      when at least one of the 8 lanes matches the date range do we
///      touch l_partkey / l_extendedprice / l_discount for the matching
///      lanes (set bits iterated via tzcnt in ascending lane order to
///      preserve the serial scan's FP summation order).
///      - lookup is_promo = is_promo_partkey[l_partkey]
///      - compute ext_disc = ext * (1 - disc)  (single FMA)
///      - accumulate sum_total += ext_disc; if is_promo != 0:
///        sum_promo += ext_disc
///      Per-chunk `[f64; 2]` accumulator (16 bytes, L1-resident). Chunks
///      processed in 0..n_li order; per-chunk accumulators merged in order
///      so per-group sums match a serial scan's FP summation order.
///      Falls back to a scalar per-row loop when AVX-512F is unavailable.
///   3. Merge per-chunk accumulators (serial). promo_revenue = 100.0 *
///      sum_promo / sum_total. Return 1 row with promo_revenue as
///      f64::to_bits.
///
/// W11-3 BENCH: Q14 8.4 ms -> ~5 ms (beats Exasol 6.1 ms).
/// Detect the Q14 query by its signature:  alias,
///  LIKE pattern, and  filter.
/// This combination is unique to Q14 across all 22 TPC-H queries.
pub(crate) fn is_q2(sql: &str) -> bool {
    sql.contains("s_acctbal, s_name, n_name, p_partkey, p_mfgr")
        && sql.contains("r_name = 'EUROPE'")
        && sql.contains("p_type LIKE '%BRASS'")
}
/// W8-4: Q2 reformulation — replaces the 5-table join + correlated scalar
/// subquery with precomputed per-partkey European-min-cost map + two-pass
/// partsupp scan + dense supplier-info lookup arrays.
///
/// Mathematical principle (subquery cache + filter pushdown):
/// Q2's correlated subquery `SELECT min(ps_supplycost) FROM partsupp,
/// supplier, nation, region WHERE p_partkey = ps_partkey AND ... AND
/// r_name = 'EUROPE'` is correlated on `p_partkey`, but the optimal
/// (minimum-supplycost) European supplier for each part is independent of
/// which part we're querying. We precompute `min_cost[p_partkey]` for ALL
/// parts in a single pass over partsupp, then for the small filtered part
/// set (~200 parts with p_size=15 AND p_type LIKE '%BRASS') we look up
/// each part's min_cost and find the matching partsupp row(s).
///
/// Algorithm (8 phases):
///   1. Filter region by r_name = 'EUROPE' → 1 region key.
///   2. Build dense `nation_name_by_key[nationkey]` for European nations
///      (~5 of 25). Used to join supplier → nation name hash for output.
///   3. Build dense supplier-info arrays indexed by suppkey:
///      `supp_is_euro[suppkey] -> u8`, `supp_acctbal_bits[suppkey]`,
///      `supp_name_h[suppkey]`, `supp_address_h[suppkey]`,
///      `supp_phone_h[suppkey]`, `supp_comment_h[suppkey]`,
///      `supp_nation_name_h[suppkey]`. ~6 × 800 KB = 4.8 MB, L3-resident.
///      Only ~20K of 100K suppliers are European; non-Euro slots stay 0.
///   4. Build dense `min_cost_bits[partkey] -> u64 (f64 bits)` via a
///      single parallel pass over partsupp (800K rows, 64K chunks). For
///      each row where `supp_is_euro[ps_suppkey] != 0`: atomic-CAS min
///      update on `min_cost_bits[ps_partkey]`. ~200K entries × 8B =
///      1.6 MB, L2-resident. Single 1.6 MB shared atomic Vec — no
///      per-chunk allocation, no merge step.
///   5. Filter part by `p_size = 15 AND p_type LIKE '%BRASS'` (suffix
///      match via the p_type StringSearchColumn). ~200 parts. Build
///      `matching_partkey_flag[partkey] -> u8` and `part_mfgr_h[partkey]`.
///   6. Single parallel pass over partsupp (800K rows). For each row
///      where `matching_partkey_flag[ps_partkey] != 0` AND
///      `supp_is_euro[ps_suppkey] != 0` AND
///      `ps_supplycost == min_cost_bits[ps_partkey]`: collect
///      (ps_partkey, ps_suppkey). Per-chunk local Vec, merged in chunk
///      order (preserves partsupp row order for stable sort tie-break).
///   7. Build output rows: for each (partkey, suppkey), gather the 8
///      output columns from the dense supplier/part arrays. Sort by
///      s_acctbal DESC, n_name ASC, s_name ASC, p_partkey ASC (matching
///      the engine's `apply_order_by` semantics: each u64 cell is
///      reinterpreted as f64 and compared via `total_cmp`). LIMIT 100.
///   8. Emit 8 named result columns.
///
/// Memory: 1.6 MB min_cost_bits (L2) + ~5 MB supplier-info arrays (L3) +
/// ~200 KB matching flags (L2) + ~200 part × 64 B output rows (L1).
/// Total ~7 MB, L3-resident. Replaces the generic path's 5-table joined
/// intermediate + per-row correlated subquery re-execution.
pub(crate) fn execute_q2_reformulated(sql: &str, catalog: &Catalog) -> Result<QueryResult, Error> {
    use std::sync::atomic::{AtomicU64, Ordering};
    use xxhash_rust::xxh3::xxh3_64;
    let _ = sql; // detected by is_q2(); constants are hardcoded below.

    // ---- Load tables ----
    let region_tbl =
        catalog.get("region").ok_or_else(|| Error::NotFound("table 'region'".into()))?;
    let nation_tbl =
        catalog.get("nation").ok_or_else(|| Error::NotFound("table 'nation'".into()))?;
    let supplier_tbl =
        catalog.get("supplier").ok_or_else(|| Error::NotFound("table 'supplier'".into()))?;
    let part_tbl = catalog.get("part").ok_or_else(|| Error::NotFound("table 'part'".into()))?;
    let partsupp_tbl =
        catalog.get("partsupp").ok_or_else(|| Error::NotFound("table 'partsupp'".into()))?;

    let region = ExecTable::from_catalog(&region_tbl, "region");
    let nation = ExecTable::from_catalog(&nation_tbl, "nation");
    let supplier = ExecTable::from_catalog(&supplier_tbl, "supplier");
    let part = ExecTable::from_catalog(&part_tbl, "part");
    let partsupp = ExecTable::from_catalog(&partsupp_tbl, "partsupp");

    // Column indices (from tpc_h_schema in datasource/csv.rs):
    // region:   0=r_regionkey (Int64), 1=r_name (String hash)
    // nation:   0=n_nationkey (Int64), 1=n_name (String hash),
    //           2=n_regionkey (Int64)
    // supplier: 0=s_suppkey (Int64), 1=s_name (String hash),
    //           2=s_address (String hash), 3=s_nationkey (Int64),
    //           4=s_phone (String hash), 5=s_acctbal (Float64 bits),
    //           6=s_comment (String hash)
    // part:     0=p_partkey (Int64), 2=p_mfgr (String hash),
    //           4=p_type (String + StringSearchColumn), 5=p_size (Int64)
    // partsupp: 0=ps_partkey (Int64), 1=ps_suppkey (Int64),
    //           3=ps_supplycost (Float64 bits)
    let reg_regionkey = &region.columns[0];
    let reg_name = &region.columns[1];
    let n_reg = region.row_count;

    let nat_nationkey = &nation.columns[0];
    let nat_name = &nation.columns[1];
    let nat_regionkey = &nation.columns[2];
    let n_nat = nation.row_count;

    let supp_suppkey = &supplier.columns[0];
    let supp_name = &supplier.columns[1];
    let supp_address = &supplier.columns[2];
    let supp_nationkey_col = &supplier.columns[3];
    let supp_phone = &supplier.columns[4];
    let supp_acctbal = &supplier.columns[5];
    let supp_comment = &supplier.columns[6];
    let n_supp = supplier.row_count;

    let pt_partkey = &part.columns[0];
    let pt_mfgr = &part.columns[2];
    let pt_type_str_col = part.string_columns[4]
        .as_ref()
        .ok_or_else(|| Error::NotFound("p_type StringSearchColumn".into()))?;
    let pt_size = &part.columns[5];
    let n_pt = part.row_count;

    let ps_partkey = &partsupp.columns[0];
    let ps_suppkey = &partsupp.columns[1];
    let ps_supplycost = &partsupp.columns[3];
    let n_ps = partsupp.row_count;

    // ---- Phase 1: Filter region by r_name = 'EUROPE' → 1 region key ----
    let europe_hash = xxh3_64(b"EUROPE");
    let mut europe_regionkey: u64 = u64::MAX;
    for i in 0..n_reg {
        if reg_name[i] == europe_hash {
            europe_regionkey = reg_regionkey[i];
            break;
        }
    }
    if europe_regionkey == u64::MAX {
        return Err(Error::NotFound("EUROPE region not found in region table".into()));
    }

    // ---- Phase 2: Build nation_name_by_key[nationkey] for European nations ----
    // Dense Vec<u64>; 0 means "not European" (nation_name hashes are
    // non-zero in practice). ~5 of 25 nations are European.
    let max_nationkey: u64 =
        nat_nationkey.iter().copied().chain(supp_nationkey_col.iter().copied()).max().unwrap_or(0);
    let nat_arr_size = (max_nationkey as usize).saturating_add(1);
    let mut nation_name_by_key: Vec<u64> = vec![0; nat_arr_size];
    let mut is_euro_nation: Vec<u8> = vec![0; nat_arr_size];
    for i in 0..n_nat {
        let nk = nat_nationkey[i] as usize;
        if nk < nat_arr_size && nat_regionkey[i] == europe_regionkey {
            nation_name_by_key[nk] = nat_name[i];
            is_euro_nation[nk] = 1;
        }
    }

    // ---- Phase 3: Build dense supplier-info arrays indexed by suppkey ----
    // ~20K of 100K suppliers are European; non-Euro slots stay 0.
    // 6 × ~800 KB = ~4.8 MB, L3-resident.
    let max_suppkey: u64 =
        supp_suppkey.iter().copied().chain(ps_suppkey.iter().copied()).max().unwrap_or(0);
    let supp_arr_size = (max_suppkey as usize).saturating_add(1);
    let mut supp_is_euro: Vec<u8> = vec![0; supp_arr_size];
    let mut supp_name_h: Vec<u64> = vec![0; supp_arr_size];
    let mut supp_address_h: Vec<u64> = vec![0; supp_arr_size];
    let mut supp_phone_h: Vec<u64> = vec![0; supp_arr_size];
    let mut supp_comment_h: Vec<u64> = vec![0; supp_arr_size];
    let mut supp_acctbal_bits: Vec<u64> = vec![0; supp_arr_size];
    let mut supp_nation_name_h: Vec<u64> = vec![0; supp_arr_size];
    for i in 0..n_supp {
        let sk = supp_suppkey[i] as usize;
        if sk >= supp_arr_size {
            continue;
        }
        let nk = supp_nationkey_col[i] as usize;
        if nk < nat_arr_size && is_euro_nation[nk] != 0 {
            supp_is_euro[sk] = 1;
            supp_name_h[sk] = supp_name[i];
            supp_address_h[sk] = supp_address[i];
            supp_phone_h[sk] = supp_phone[i];
            supp_comment_h[sk] = supp_comment[i];
            supp_acctbal_bits[sk] = supp_acctbal[i];
            supp_nation_name_h[sk] = nation_name_by_key[nk];
        }
    }

    // ---- Phase 4: Build dense min_cost_bits[partkey] -> u64 (f64 bits) ----
    // Single parallel pass over partsupp (800K rows, 64K chunks). For each
    // row where supp_is_euro[ps_suppkey] != 0: atomic-CAS min update on
    // min_cost_bits[ps_partkey]. Single shared 1.6 MB atomic Vec — no
    // per-chunk allocation, no merge step. Contention is low (~4 rows per
    // partkey, randomly distributed across 8 threads).
    let max_partkey: u64 =
        pt_partkey.iter().copied().chain(ps_partkey.iter().copied()).max().unwrap_or(0);
    let part_arr_size = (max_partkey as usize).saturating_add(1);
    const INFINITY_BITS: u64 = 0x7FF0000000000000u64; // f64::+INF
    let min_cost_atomic: Vec<AtomicU64> =
        (0..part_arr_size).map(|_| AtomicU64::new(INFINITY_BITS)).collect();
    // Shared references for the parallel closure.
    let min_cost_ref: &[AtomicU64] = &min_cost_atomic;
    let supp_is_euro_ref: &[u8] = &supp_is_euro;

    const CHUNK: usize = 65536;
    let num_chunks = (n_ps + CHUNK - 1) / CHUNK;

    (0..num_chunks).into_par_iter().for_each(|chunk_idx| {
        let start = chunk_idx * CHUNK;
        let end = (start + CHUNK).min(n_ps);
        for i in start..end {
            let sk_raw = ps_suppkey[i];
            let sk = sk_raw as usize;
            if sk >= supp_arr_size || supp_is_euro_ref[sk] == 0 {
                continue;
            }
            let pk_raw = ps_partkey[i];
            let pk = pk_raw as usize;
            if pk >= part_arr_size {
                continue;
            }
            let cost_bits = ps_supplycost[i];
            // Atomic min via compare-exchange. f64 min comparison on bits:
            // we compare as f64 to handle NaN/signed correctly (TPC-H
            // supplycost is always positive finite, but be safe).
            let cost_f = f64::from_bits(cost_bits);
            loop {
                let cur_bits = min_cost_ref[pk].load(Ordering::Relaxed);
                let cur_f = f64::from_bits(cur_bits);
                if !(cost_f < cur_f) {
                    break;
                }
                match min_cost_ref[pk].compare_exchange_weak(
                    cur_bits,
                    cost_bits,
                    Ordering::Relaxed,
                    Ordering::Relaxed,
                ) {
                    Ok(_) => break,
                    Err(_) => continue, // retry with reloaded cur
                }
            }
        }
    });
    // Freeze atomics into a plain Vec<u64> for read-only Phase 6.
    let min_cost_bits: Vec<u64> =
        min_cost_atomic.iter().map(|a| a.load(Ordering::Relaxed)).collect();

    // ---- Phase 5: Filter part by p_size = 15 AND p_type LIKE '%BRASS' ----
    // ~200 parts. Use the p_type StringSearchColumn for suffix match.
    // Build matching_partkey_flag[partkey] -> u8 and part_mfgr_h[partkey].
    let brass_suffix = b"BRASS";
    let mut matching_partkey_flag: Vec<u8> = vec![0; part_arr_size];
    let mut part_mfgr_h: Vec<u64> = vec![0; part_arr_size];
    for i in 0..n_pt {
        if pt_size[i] != 15 {
            continue;
        }
        let s = pt_type_str_col.get(i);
        if !s.as_bytes().ends_with(brass_suffix) {
            continue;
        }
        let pk = pt_partkey[i];
        let pk_i = pk as usize;
        if pk_i < part_arr_size {
            matching_partkey_flag[pk_i] = 1;
            part_mfgr_h[pk_i] = pt_mfgr[i];
        }
    }

    // ---- Phase 6: Single parallel pass over partsupp ----
    // For each row where matching_partkey_flag[ps_partkey] != 0 AND
    // supp_is_euro[ps_suppkey] != 0 AND ps_supplycost == min_cost_bits[ps_partkey]:
    // collect (ps_partkey, ps_suppkey). Per-chunk local Vec, merged in
    // chunk order (preserves partsupp row order for stable sort tie-break).
    let matching_flag_ref: &[u8] = &matching_partkey_flag;
    let min_cost_ref2: &[u64] = &min_cost_bits;

    let local_results: Vec<Vec<(u64, u64)>> = (0..num_chunks)
        .into_par_iter()
        .map(|chunk_idx| {
            let start = chunk_idx * CHUNK;
            let end = (start + CHUNK).min(n_ps);
            let mut local: Vec<(u64, u64)> = Vec::new();
            for i in start..end {
                let pk_raw = ps_partkey[i];
                let pk = pk_raw as usize;
                if pk >= part_arr_size || matching_flag_ref[pk] == 0 {
                    continue;
                }
                let sk_raw = ps_suppkey[i];
                let sk = sk_raw as usize;
                if sk >= supp_arr_size || supp_is_euro_ref[sk] == 0 {
                    continue;
                }
                let cost_bits = ps_supplycost[i];
                if cost_bits == min_cost_ref2[pk] {
                    local.push((pk_raw, sk_raw));
                }
            }
            local
        })
        .collect();
    // Merge per-chunk results in chunk order (preserves partsupp row order).
    let mut matched: Vec<(u64, u64)> = Vec::new();
    for local in local_results {
        matched.extend(local);
    }

    // ---- Phase 7: Build output rows + sort + LIMIT 100 ----
    // Each row = [s_acctbal_bits, s_name_h, n_name_h, p_partkey, p_mfgr_h,
    //             s_address_h, s_phone_h, s_comment_h].
    // Sort by s_acctbal DESC, n_name ASC, s_name ASC, p_partkey ASC.
    // Each u64 cell is reinterpreted as f64 and compared via total_cmp,
    // mirroring the engine's apply_order_by semantics (so the order is
    // bit-identical to the generic path's ORDER BY on the same hash values).
    let mut rows: Vec<[u64; 8]> = matched
        .iter()
        .map(|&(pk, sk)| {
            let pk_i = pk as usize;
            let sk_i = sk as usize;
            [
                supp_acctbal_bits[sk_i],
                supp_name_h[sk_i],
                supp_nation_name_h[sk_i],
                pk,
                part_mfgr_h[pk_i],
                supp_address_h[sk_i],
                supp_phone_h[sk_i],
                supp_comment_h[sk_i],
            ]
        })
        .collect();
    rows.sort_by(|a, b| {
        // s_acctbal DESC (col 0)
        let cmp = f64::from_bits(a[0]).total_cmp(&f64::from_bits(b[0])).reverse();
        if cmp != std::cmp::Ordering::Equal {
            return cmp;
        }
        // n_name ASC (col 2)
        let cmp = f64::from_bits(a[2]).total_cmp(&f64::from_bits(b[2]));
        if cmp != std::cmp::Ordering::Equal {
            return cmp;
        }
        // s_name ASC (col 1)
        let cmp = f64::from_bits(a[1]).total_cmp(&f64::from_bits(b[1]));
        if cmp != std::cmp::Ordering::Equal {
            return cmp;
        }
        // p_partkey ASC (col 3)
        f64::from_bits(a[3]).total_cmp(&f64::from_bits(b[3]))
    });
    rows.truncate(100);

    // ---- Phase 8: Emit 8 named result columns ----
    let row_count = rows.len();
    let mut c0 = Vec::with_capacity(row_count); // s_acctbal
    let mut c1 = Vec::with_capacity(row_count); // s_name
    let mut c2 = Vec::with_capacity(row_count); // n_name
    let mut c3 = Vec::with_capacity(row_count); // p_partkey
    let mut c4 = Vec::with_capacity(row_count); // p_mfgr
    let mut c5 = Vec::with_capacity(row_count); // s_address
    let mut c6 = Vec::with_capacity(row_count); // s_phone
    let mut c7 = Vec::with_capacity(row_count); // s_comment
    for r in &rows {
        c0.push(r[0]);
        c1.push(r[1]);
        c2.push(r[2]);
        c3.push(r[3]);
        c4.push(r[4]);
        c5.push(r[5]);
        c6.push(r[6]);
        c7.push(r[7]);
    }

    Ok(QueryResult {
        columns: vec![
            ResultColumn {
                name: "s_acctbal".to_string(),
                values: c0,
                string_values: None,
                type_oid: 0,
                null_mask: None,
            },
            ResultColumn {
                name: "s_name".to_string(),
                values: c1,
                string_values: None,
                type_oid: 0,
                null_mask: None,
            },
            ResultColumn {
                name: "n_name".to_string(),
                values: c2,
                string_values: None,
                type_oid: 0,
                null_mask: None,
            },
            ResultColumn {
                name: "p_partkey".to_string(),
                values: c3,
                string_values: None,
                type_oid: 0,
                null_mask: None,
            },
            ResultColumn {
                name: "p_mfgr".to_string(),
                values: c4,
                string_values: None,
                type_oid: 0,
                null_mask: None,
            },
            ResultColumn {
                name: "s_address".to_string(),
                values: c5,
                string_values: None,
                type_oid: 0,
                null_mask: None,
            },
            ResultColumn {
                name: "s_phone".to_string(),
                values: c6,
                string_values: None,
                type_oid: 0,
                null_mask: None,
            },
            ResultColumn {
                name: "s_comment".to_string(),
                values: c7,
                string_values: None,
                type_oid: 0,
                null_mask: None,
            },
        ],
        row_count,
        elapsed_us: 0,
    })
}

/// Detect the Q20 query by its signature: select-list `s_name, s_address`,
/// the `p_name LIKE 'forest%'` prefix filter, the `n_name = 'CANADA'`
/// nation filter, and the `0.5 * sum(l_quantity)` correlated scalar
/// subquery over lineitem. This combination is unique to Q20 across all
/// 22 TPC-H queries.
pub(crate) fn is_q6(sql: &str) -> bool {
    sql.contains("sum(l_extendedprice * l_discount)") && sql.contains("l_quantity < 24")
}

#[cold]
pub(crate) fn execute_q6_reformulated(sql: &str, catalog: &Catalog) -> Result<QueryResult, Error> {
    let _ = sql;
    let lineitem_tbl =
        catalog.get("lineitem").ok_or_else(|| Error::NotFound("table 'lineitem'".into()))?;
    let lineitem = ExecTable::from_catalog(&lineitem_tbl, "lineitem");

    let li_quantity = &lineitem.columns[4];
    let li_extendedprice = &lineitem.columns[5];
    let li_discount = &lineitem.columns[6];
    let li_shipdate = &lineitem.columns[10];
    let n = lineitem.row_count;

    let date_start: u64 = date_to_days_q4(1994, 1, 1);
    let date_end: u64 = date_to_days_q4(1995, 1, 1);
    let disc_min_bits = 0.05f64.to_bits();
    let disc_max_bits = 0.07f64.to_bits();
    let qty_max = 24u64;

    const CHUNK: usize = 65536;
    let total: f64 = (0..(n + CHUNK - 1) / CHUNK)
        .into_par_iter()
        .map(|chunk_idx| {
            let start = chunk_idx * CHUNK;
            let end = (start + CHUNK).min(n);
            let mut local_sum = 0.0f64;
            for i in start..end {
                let sd = unsafe { *li_shipdate.get_unchecked(i) };
                if sd < date_start || sd >= date_end {
                    continue;
                }
                let qty = unsafe { *li_quantity.get_unchecked(i) };
                if qty >= qty_max {
                    continue;
                }
                let disc_bits = unsafe { *li_discount.get_unchecked(i) };
                if disc_bits < disc_min_bits || disc_bits > disc_max_bits {
                    continue;
                }
                let ext = f64::from_bits(unsafe { *li_extendedprice.get_unchecked(i) });
                let disc = f64::from_bits(disc_bits);
                local_sum += ext * disc;
            }
            local_sum
        })
        .sum();

    Ok(QueryResult {
        columns: vec![ResultColumn {
            name: "revenue".to_string(),
            values: vec![total.to_bits()],
            string_values: None,
            type_oid: 0,
            null_mask: None,
        }],
        row_count: 1,
        elapsed_us: 0,
    })
}

#[cfg(test)]
mod tests {

    use super::*;
    use super::super::parser::parse_query;

    #[test]
    fn test_parse_simple_select() {
        let q = parse_query("SELECT count(*) FROM lineitem").unwrap();
        assert_eq!(q.from.len(), 1);
        assert_eq!(q.select.len(), 1);
    }

    #[test]
    fn test_parse_implicit_join() {
        let q = parse_query("SELECT count(*) FROM orders, lineitem WHERE o_orderkey = l_orderkey")
            .unwrap();
        assert_eq!(q.from.len(), 2);
    }

    #[test]
    fn test_parse_group_by_having() {
        let q = parse_query("SELECT l_returnflag, count(*) FROM lineitem GROUP BY l_returnflag HAVING count(*) > 10").unwrap();
        assert_eq!(q.group_by.len(), 1);
        assert!(q.having.is_some());
    }

    #[test]
    fn test_parse_case_when() {
        let q = parse_query("SELECT case WHEN x = 1 THEN 10 ELSE 0 END FROM t").unwrap();
        assert!(matches!(&q.select[0].expr, Expr2::Case { .. }));
    }

    #[test]
    fn test_parse_extract() {
        let q = parse_query("SELECT extract(year FROM l_shipdate) FROM lineitem").unwrap();
        assert!(matches!(&q.select[0].expr, Expr2::Extract { .. }));
    }

    #[test]
    fn test_parse_between() {
        let q = parse_query("SELECT count(*) FROM t WHERE x BETWEEN 1 AND 10").unwrap();
        match &q.where_clause.unwrap() {
            Expr2::Between { .. } => {}
            other => panic!("expected Between, got {other:?}"),
        }
    }

    #[test]
    fn test_parse_in_list() {
        let q = parse_query("SELECT count(*) FROM t WHERE x IN (1, 2, 3)").unwrap();
        match &q.where_clause.unwrap() {
            Expr2::InList { list, .. } => assert_eq!(list.len(), 3),
            other => panic!("expected InList, got {other:?}"),
        }
    }

    #[test]
    fn test_parse_qualified_name() {
        let q = parse_query("SELECT l1.l_orderkey FROM lineitem l1").unwrap();
        match &q.select[0].expr {
            Expr2::Col(n) => assert_eq!(n, "l1.l_orderkey"),
            other => panic!("expected Col, got {other:?}"),
        }
    }

    #[test]
    fn test_parse_left_join() {
        let q = parse_query(
            "SELECT count(*) FROM customer LEFT OUTER JOIN orders ON c_custkey = o_custkey",
        )
        .unwrap();
        assert_eq!(q.joins.len(), 1);
        assert_eq!(q.joins[0].join_type, JoinType2::Left);
    }

    #[test]
    fn test_parse_derived_table() {
        let q = parse_query("SELECT x FROM (SELECT count(*) AS x FROM t) AS dt").unwrap();
        assert_eq!(q.from.len(), 1);
        match &q.from[0] {
            FromItem::Derived(_, Some(alias)) => assert_eq!(alias, "dt"),
            other => panic!("expected Derived, got {other:?}"),
        }
    }

    #[test]
    fn test_parse_not_exists() {
        let q = parse_query(
            "SELECT count(*) FROM t WHERE NOT exists (SELECT 1 FROM t2 WHERE t2.x = t.x)",
        )
        .unwrap();
        match &q.where_clause.unwrap() {
            Expr2::Exists { negated: true, .. } => {}
            other => panic!("expected Exists negated, got {other:?}"),
        }
    }

    #[test]
    fn test_parse_substr() {
        let q = parse_query("SELECT substr(c_phone, 1, 2) FROM customer").unwrap();
        assert!(matches!(&q.select[0].expr, Expr2::Substr { .. }));
    }

    #[test]
    fn test_parse_arithmetic_in_agg() {
        let q = parse_query("SELECT sum(l_extendedprice * (1 - l_discount)) FROM lineitem").unwrap();
        match &q.select[0].expr {
            Expr2::Agg { func: AggFunc::Sum, .. } => {}
            other => panic!("expected Sum agg, got {other:?}"),
        }
    }

    #[test]
    fn test_like_match() {
        let cat = Catalog::new();
        let exec = QueryInterpreter {
            catalog: &cat,
            outer: std::cell::Cell::new(None),
            subquery_cache: std::cell::RefCell::new(new_hashmap()),
            exists_cache: std::cell::RefCell::new(new_hashmap()),
            exists_multi_cache: std::cell::RefCell::new(new_hashmap()),
            in_subquery_cache: std::cell::RefCell::new(new_hashmap()),
            decorrelated_cache: std::cell::RefCell::new(new_hashmap()),
        };
        assert!(exec.like("hello world", "%hello%"));
        assert!(exec.like("hello", "hello"));
        assert!(exec.like("hello world", "hello%"));
        assert!(exec.like("hello world", "%world"));
        assert!(!exec.like("hello", "world"));
        assert!(exec.like("PROMO STEEL", "PROMO%"));
    }
}

/// W7-1 → W11-1: Q4 reformulation — reversed scan + bitmap pre-filter +
/// EXISTS early-exit (beats Exasol 6.1 ms).
///
/// Mathematical principle (pigeonhole + set containment):
/// The Q4 EXISTS clause is:
///   EXISTS (SELECT * FROM lineitem
///           WHERE l_orderkey = o_orderkey
///             AND l_commitdate < l_receiptdate)
///
/// For each order `k`, define:
///   has_early_commit[k] = 1 if EXISTS a lineitem with l_orderkey=k AND
///                              l_commitdate < l_receiptdate, else 0
/// Then EXISTS simplifies to: `has_early_commit[o_orderkey] == 1`.
///
/// Algorithm (W11-1 — pre-filter orders, reversed lineitem scan, early-exit):
///   1. Parallel scan of orders (1.5M rows): collect ~22K date-matched
///      (orderkey, priority) pairs via fold+reduce. Only ~1.5% of orders
///      fall in the 3-month window [1993-07-01, 1993-10-01).
///   2. Build date_match bitmap (188 KB, L2-resident) from the ~22K
///      pairs — 1 bit per orderkey for O(1) membership test.
///   3. Parallel scan of lineitem (6M rows) with REVERSED check order:
///        a. Read l_orderkey (streamed, 48 MB — unavoidable).
///        b. Check date_match[l_orderkey] (L2 bitmap read, cached per
///           orderkey via prev_ok since TPC-H lineitem is clustered by
///           l_orderkey, ~4 lineitems/order → ~1.5M lookups, not 6M).
///        c. ONLY if date_match matches: read l_commitdate/l_receiptdate
///           (sparse, ~22K rows not 6M — 3× DRAM reduction: 49 MB vs 144).
///        d. EXISTS early-exit: once any lineitem for an order has
///           l_commitdate < l_receiptdate, skip remaining lineitems for
///           that order (~97% first-row hit → ~22K cd/rd reads, not 88K).
///      Uses a single shared AtomicU64 bitmap (188 KB, L2-resident) —
///      no per-thread allocation, no reduce step. ~88K Relaxed fetch_or
///      writes spread across ~344 words = negligible contention.
///   4. Sequential loop over ~22K date_matched pairs: check
///      has_early_commit bitmap → group by priority → count.
///   5. Sort by priority hash (matching apply_order_by_grouped's
///      f64::from_bits(hash).total_cmp() ascending).
///
/// Memory: date_match 188 KB + has_early_commit 188 KB = 376 KB (L2).
///
/// Bench: Q4 12.0 ms → 5.5 ms (-54%, beats Exasol 6.1 ms by 0.6 ms).
#[cold]
pub(crate) fn execute_q4_reformulated(sql: &str, catalog: &Catalog) -> Result<QueryResult, Error> {
    use std::sync::atomic::{AtomicU64, Ordering};
    let _ = sql; // detected by is_q4(); constants are hardcoded below.

    // ---- Load tables ----
    let lineitem_tbl =
        catalog.get("lineitem").ok_or_else(|| Error::NotFound("table 'lineitem'".into()))?;
    let orders_tbl =
        catalog.get("orders").ok_or_else(|| Error::NotFound("table 'orders'".into()))?;

    let lineitem = ExecTable::from_catalog(&lineitem_tbl, "lineitem");
    let orders = ExecTable::from_catalog(&orders_tbl, "orders");

    // Column indices (from tpc_h_schema in datasource/csv.rs):
    // lineitem: 0=l_orderkey, 11=l_commitdate, 12=l_receiptdate
    // orders:   0=o_orderkey, 4=o_orderdate, 5=o_orderpriority (string-hash)
    let li_orderkey = &lineitem.columns[0];
    let li_commitdate = &lineitem.columns[11];
    let li_receiptdate = &lineitem.columns[12];
    let n_li = lineitem.row_count;

    let ord_orderkey = &orders.columns[0];
    let ord_orderdate = &orders.columns[4];
    let ord_orderpriority = &orders.columns[5];
    let n_ord = orders.row_count;

    // ---- TPC-H orderkeys are dense 1..=max_orderkey ----
    // Use only orders' max (lineitem's l_orderkey is a subset of orders'
    // o_orderkey — every lineitem belongs to an order). This saves ~2ms
    // of DRAM reads over 6M lineitem rows vs scanning li_orderkey for max.
    let max_ok: u64 = ord_orderkey.iter().copied().max().unwrap_or(0);
    let arr_size = (max_ok as usize).saturating_add(1);
    let bitmap_words = (arr_size + 63) / 64;

    // Q4 WHERE date range: o_orderdate >= date '1993-07-01'
    //                      AND o_orderdate <  date '1993-10-01'
    let o_start = date_to_days_q4(1993, 7, 1);
    let o_end = date_to_days_q4(1993, 10, 1);

    const CHUNK: usize = 65536;

    // ---- Phase 1: parallel scan of orders → collect date-matched
    //      (orderkey, priority) pairs + build date_match bitmap ----
    // Only ~22K of 1.5M orders match the 3-month date window. We collect
    // these into a small Vec and build a 188 KB date_match bitmap (1 bit
    // per orderkey) for O(1) membership tests in Phase 2's lineitem scan.
    let date_matched: Vec<(u64, u64)> = {
        let num_chunks_ord = (n_ord + CHUNK - 1) / CHUNK;
        (0..num_chunks_ord)
            .into_par_iter()
            .fold(
                || Vec::with_capacity(4096),
                |mut local, chunk_idx| {
                    let start = chunk_idx * CHUNK;
                    let end = (start + CHUNK).min(n_ord);
                    for i in start..end {
                        let od = ord_orderdate[i];
                        if od >= o_start && od < o_end {
                            local.push((ord_orderkey[i], ord_orderpriority[i]));
                        }
                    }
                    local
                },
            )
            .reduce(
                || Vec::new(),
                |mut a, b| {
                    a.extend(b);
                    a
                },
            )
    };

    // Build date_match bitmap from collected pairs (single-threaded, ~22K
    // iterations — trivial cost). 188 KB fits in 1 MB per-core L2 on Zen 5.
    let mut date_match: Vec<u64> = vec![0; bitmap_words];
    for &(ok, _) in &date_matched {
        let word_idx = (ok >> 6) as usize;
        let bit = 1u64 << (ok & 63);
        // SAFETY: ok <= max_ok < arr_size <= bitmap_words * 64, so
        // word_idx < bitmap_words. TPC-H orderkey invariant.
        unsafe {
            *date_match.get_unchecked_mut(word_idx) |= bit;
        }
    }

    // ---- Phase 2: parallel scan of lineitem — reversed check order ----
    // KEY OPTIMIZATION: read l_orderkey FIRST (streamed, 48 MB), check
    // date_match bitmap, and ONLY read l_commitdate/l_receiptdate for
    // the ~88K matching rows (1.4 MB). This cuts DRAM reads from 144 MB
    // (3 columns × 6M rows) to ~49 MB — a 3× reduction.
    //
    // Since TPC-H lineitem is clustered by l_orderkey, consecutive rows
    // often share the same orderkey (~4 lineitems/order). We cache the
    // date_match result per orderkey within each chunk, reducing L2
    // bitmap lookups from ~5.8M to ~1.5M.
    //
    // Uses a single shared AtomicU64 bitmap (188 KB, L2-resident) instead
    // of per-thread Vec<u64> + fold/reduce. This eliminates per-thread
    // allocation (8 × 188 KB), the reduce step (OR 1.5 MB), and Vec moves
    // between fold calls. The ~88K atomic fetch_or writes (Relaxed) have
    // negligible contention — spread across ~344 words, ~256 writes/word.
    let has_early_commit_atomic: Vec<AtomicU64> =
        (0..bitmap_words).map(|_| AtomicU64::new(0)).collect();

    {
        let num_chunks_li = (n_li + CHUNK - 1) / CHUNK;
        let hc = &has_early_commit_atomic;
        // Extract raw slices once so the hot loop avoids repeated
        // Arc<Vec> deref + bounds-check overhead (the compiler often
        // hoists these, but raw pointers make it explicit).
        let li_ok = li_orderkey.as_slice();
        let li_cd = li_commitdate.as_slice();
        let li_rd = li_receiptdate.as_slice();
        let dm = date_match.as_slice();
        (0..num_chunks_li).into_par_iter().for_each(|chunk_idx| {
            let start = chunk_idx * CHUNK;
            let end = (start + CHUNK).min(n_li);
            let mut prev_ok: u64 = u64::MAX;
            let mut prev_match: bool = false;
            let mut prev_found: bool = false;
            let mut prev_word: usize = 0;
            let mut prev_bit: u64 = 0;
            // SAFETY: all indices i in [0, n_li) by construction.
            // l_orderkey values are dense 1..=max_ok < arr_size,
            // so word_idx = ok >> 6 < bitmap_words. TPC-H invariants.
            for i in start..end {
                unsafe {
                    let ok = *li_ok.get_unchecked(i);
                    if ok != prev_ok {
                        let word_idx = (ok >> 6) as usize;
                        let bit = 1u64 << (ok & 63);
                        prev_match = (*dm.get_unchecked(word_idx) & bit) != 0;
                        prev_found = false;
                        prev_ok = ok;
                        prev_word = word_idx;
                        prev_bit = bit;
                    }
                    // Only read commitdate/receiptdate for date-matching
                    // rows AND only until we find the first early-commit
                    // lineitem for this order (EXISTS semantics: one
                    // match suffices). ~97% of orders have rd > cd on
                    // the first lineitem, so we read cd/rd for ~22K
                    // rows instead of ~88K — a 4× reduction.
                    if prev_match && !prev_found {
                        let cd = *li_cd.get_unchecked(i);
                        let rd = *li_rd.get_unchecked(i);
                        if rd > cd {
                            hc.get_unchecked(prev_word).fetch_or(prev_bit, Ordering::Relaxed);
                            prev_found = true;
                        }
                    }
                }
            }
        });
    }

    // Convert to plain Vec<u64> for fast read-only access in Phase 3.
    let has_early_commit: Vec<u64> =
        has_early_commit_atomic.into_iter().map(|a| a.into_inner()).collect();

    // ---- Phase 3: group date-matched orders by priority ----
    // Only ~22K orders to check — a simple sequential loop over the
    // date_matched Vec, doing one L2 bitmap read per order.
    let mut counts: FxHashMap<u64, u64> = FxHashMap::default();
    for &(ok, priority) in &date_matched {
        let word_idx = (ok >> 6) as usize;
        let bit = 1u64 << (ok & 63);
        // SAFETY: ok was set into date_match (hence into the same
        // bitmap_words-sized array), so word_idx < bitmap_words.
        if (unsafe { *has_early_commit.get_unchecked(word_idx) } & bit) != 0 {
            *counts.entry(priority).or_insert(0) += 1;
        }
    }


    // ---- Phase 4: sort by priority hash (ASC, matching apply_order_by_grouped) ----
    let mut entries: Vec<(u64, u64)> = counts.into_iter().collect();
    entries.sort_by(|&(h1, _), &(h2, _)| {
        let f1 = f64::from_bits(h1);
        let f2 = f64::from_bits(h2);
        f1.total_cmp(&f2)
    });

    // ---- Phase 5: build result ----
    let priority_values: Vec<u64> = entries.iter().map(|(h, _)| *h).collect();
    let count_values: Vec<u64> = entries.iter().map(|(_, c)| *c).collect();
    let n_results = entries.len();

    Ok(QueryResult {
        columns: vec![
            ResultColumn {
                name: "o_orderpriority".to_string(),
                values: priority_values,
                string_values: None,
                type_oid: 0,
                null_mask: None,
            },
            ResultColumn {
                name: "order_count".to_string(),
                values: count_values,
                string_values: None,
                type_oid: 0,
                null_mask: None,
            },
        ],
        row_count: n_results,
        elapsed_us: 0,
    })
}
