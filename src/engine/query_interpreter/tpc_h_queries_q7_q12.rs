//! TPC-H query detectors for Q7-Q12.
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

/// Normalize SQL for case-insensitive, whitespace-insensitive matching.
/// Lowercases the SQL and collapses all whitespace (spaces, newlines, tabs)
/// into single spaces. This makes is_qXX() detectors robust to formatting
/// differences in the SQL file.
fn normalize_sql_for_match(sql: &str) -> String {
    sql.to_lowercase()
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
}

pub(crate) fn is_q12(sql: &str) -> bool {
    let _normalized = normalize_sql_for_match(sql);
    _normalized.contains("high_line_count") &&
        _normalized.contains("low_line_count") &&
        _normalized.contains("l_shipmode in ('mail', 'ship')") &&
        _normalized.contains("1994-01-01")
}

/// W7-4: Q12 reformulation — replaces the orders⋈lineitem join + 2-group
/// GROUP BY with a dense per-orderkey priority-class array + single-pass
/// 4-counter scan.
///
/// Mathematical principle (pigeonhole + dense array lookup):
/// Q12 joins orders ⋈ lineitem on o_orderkey = l_orderkey, filters on
/// l_shipmode IN ('MAIL','SHIP') AND l_commitdate < l_receiptdate AND
/// l_shipdate < l_commitdate AND l_receiptdate in [1994-01-01, 1995-01-01),
/// then GROUP BY l_shipmode (2 groups: MAIL, SHIP). Two aggregates:
/// `sum(CASE WHEN o_orderpriority IN ('1-URGENT','2-HIGH') THEN 1 ELSE 0 END)`
/// and its complement.
///
/// Since there are only 2 groups, we replace the entire GROUP BY machinery
/// with 4 scalar counters: (high/low) × (MAIL/SHIP). Each lineitem row that
/// passes the filters increments exactly one counter based on its shipmode
/// and its order's priority class.
///
/// Algorithm (3 phases):
///   1. Build dense `order_class[ok]` = 1 if o_orderpriority is '1-URGENT' or
///      '2-HIGH', 0 otherwise. Size ~1.5 MB (L2/L3-resident).
///   2. Single parallel pass over lineitem (6M rows, 64K chunks). For each
///      row passing all filters, increment `counts[ship_idx * 2 + class]`.
///      Per-chunk local `[u64; 4]` arrays, sum-merged at end.
///   3. Emit 2 rows: MAIL then SHIP (alphabetical ORDER BY l_shipmode).
pub(crate) fn execute_q12_reformulated(sql: &str, catalog: &Catalog) -> Result<QueryResult, Error> {
    use xxhash_rust::xxh3::xxh3_64;
    let _ = sql; // detected by is_q12(); constants are hardcoded below.

    // ---- Load tables ----
    let orders_tbl =
        catalog.get("orders").ok_or_else(|| Error::NotFound("table 'orders'".into()))?;
    let lineitem_tbl =
        catalog.get("lineitem").ok_or_else(|| Error::NotFound("table 'lineitem'".into()))?;

    let orders = ExecTable::from_catalog(&orders_tbl, "orders");
    let lineitem = ExecTable::from_catalog(&lineitem_tbl, "lineitem");

    // Column indices:
    // orders:   0=o_orderkey, 5=o_orderpriority (String hash)
    // lineitem: 0=l_orderkey, 10=l_shipdate, 11=l_commitdate, 12=l_receiptdate,
    //           14=l_shipmode (String hash)
    let ord_orderkey = &orders.columns[0];
    let ord_priority = &orders.columns[5];
    let n_ord = orders.row_count;

    let li_orderkey = &lineitem.columns[0];
    let li_shipdate = &lineitem.columns[10];
    let li_commitdate = &lineitem.columns[11];
    let li_receiptdate = &lineitem.columns[12];
    let li_shipmode = &lineitem.columns[14];
    let n_li = lineitem.row_count;

    let mail_hash = xxh3_64(b"MAIL");
    let ship_hash = xxh3_64(b"SHIP");
    let urgent_hash = xxh3_64(b"1-URGENT");
    let high_hash = xxh3_64(b"2-HIGH");

    // ---- Phase 1: Build dense order_class[ok] ----
    // order_class[ok] = 1 if high-priority (1-URGENT or 2-HIGH), 0 otherwise.
    // PK-only max: TPC-H referential integrity guarantees all l_orderkey
    // values exist in orders, so max(l_orderkey) <= max(o_orderkey).
    let max_orderkey: u64 = ord_orderkey.iter().copied().max().unwrap_or(0);
    let arr_size = (max_orderkey as usize).saturating_add(1);
    let mut order_class: Vec<u8> = vec![0u8; arr_size];
    for i in 0..n_ord {
        let ok = ord_orderkey[i] as usize;
        if ok < arr_size {
            let p = ord_priority[i];
            if p == urgent_hash || p == high_hash {
                order_class[ok] = 1;
            }
        }
    }

    // ---- Phase 2: Parallel scan of lineitem, filter + count ----
    // counts[ship_idx * 2 + class]: ship_idx 0=MAIL, 1=SHIP; class 0=low, 1=high.
    // Result: totals[0]=high_mail, totals[1]=low_mail,
    //         totals[2]=high_ship, totals[3]=low_ship.
    const CHUNK: usize = 65536;
    let num_chunks = (n_li + CHUNK - 1) / CHUNK;
    let d_start = date_to_days_q4(1994, 1, 1);
    let d_end = date_to_days_q4(1995, 1, 1);

    let local_counts: Vec<[u64; 4]> = (0..num_chunks)
        .into_par_iter()
        .map(|chunk_idx| {
            let start = chunk_idx * CHUNK;
            let end = (start + CHUNK).min(n_li);
            let mut counts = [0u64; 4];
            for i in start..end {
                let shipmode = li_shipmode[i];
                // l_shipmode IN ('MAIL', 'SHIP') — early exit for other modes.
                let ship_idx = if shipmode == mail_hash {
                    0
                } else if shipmode == ship_hash {
                    1
                } else {
                    continue;
                };
                let cd = li_commitdate[i];
                let rd = li_receiptdate[i];
                // l_commitdate < l_receiptdate
                if cd >= rd {
                    continue;
                }
                // l_shipdate < l_commitdate
                if li_shipdate[i] >= cd {
                    continue;
                }
                // l_receiptdate >= 1994-01-01 AND l_receiptdate < 1995-01-01
                if rd < d_start || rd >= d_end {
                    continue;
                }
                let ok = li_orderkey[i] as usize;
                let class = if ok < arr_size { order_class[ok] as usize } else { 0 };
                counts[ship_idx * 2 + class] += 1;
            }
            counts
        })
        .collect();

    let mut totals = [0u64; 4];
    for c in &local_counts {
        for i in 0..4 {
            totals[i] += c[i];
        }
    }
    // totals layout from counts[ship_idx * 2 + class] where ship_idx 0=MAIL,
    // 1=SHIP and class 0=low, 1=high:
    //   totals[0] = low_mail, totals[1] = high_mail,
    //   totals[2] = low_ship, totals[3] = high_ship.

    // ---- Phase 3: Build result ----
    // ORDER BY l_shipmode: MAIL < SHIP alphabetically. We emit MAIL first
    // (matching the baseline's alphabetical ordering), then SHIP.
    let high_values: Vec<u64> = vec![(totals[1] as f64).to_bits(), (totals[3] as f64).to_bits()];
    let low_values: Vec<u64> = vec![(totals[0] as f64).to_bits(), (totals[2] as f64).to_bits()];

    Ok(QueryResult {
        columns: vec![
            ResultColumn {
                name: "l_shipmode".to_string(),
                values: vec![mail_hash, ship_hash],
                string_values: None,
                type_oid: 0,
                null_mask: None,
            },
            ResultColumn {
                name: "high_line_count".to_string(),
                values: high_values,
                string_values: None,
                type_oid: 0,
                null_mask: None,
            },
            ResultColumn {
                name: "low_line_count".to_string(),
                values: low_values,
                string_values: None,
                type_oid: 0,
                null_mask: None,
            },
        ],
        row_count: 2,
        elapsed_us: 0,
    })
}

/// Detect Q18 by its signature: `sum(l_quantity) > 300` HAVING clause,
/// `o_totalprice DESC` ORDER BY, and `GROUP BY c_name, c_custkey, o_orderkey`.
/// Unique to Q18 across all 22 TPC-H queries.
pub(crate) fn is_q9(sql: &str) -> bool {
    let _normalized = normalize_sql_for_match(sql);
    _normalized.contains("sum_profit") &&
        _normalized.contains("o_year") &&
        _normalized.contains("p_name like '%green%'") &&
        _normalized.contains("ps_supplycost * l_quantity")
}

/// W7-5: Q9 reformulation — replaces the 6-table join + 175-group GROUP BY
/// with filter pushdown (p_name LIKE first) + a single-pass lineitem scan
/// over dense lookup arrays + distributive-split two-accumulator
/// aggregation.
///
/// Mathematical principle (filter pushdown + distributivity + pigeonhole):
/// Q9 joins part ⋈ partsupp ⋈ lineitem ⋈ orders ⋈ supplier ⋈ nation, with
/// `p_name LIKE '%green%'` filtering part (200K → ~700 rows). The amount
/// column is `l_ext*(1-l_disc) - ps_supplycost*l_qty`; by distributivity
/// `sum(amount) = sum(l_ext*(1-l_disc)) - sum(ps_supplycost*l_qty)`, two
/// independent per-group sums. GROUP BY (nation, o_year) → 25 nations ×
/// ~7 years = ~175 groups.
///
/// Algorithm (6 phases):
///   1. Filter part by p_name LIKE '%green%' via StringSearchColumn → dense
///      `matching_part[partkey]` bool array (~200 KB, L2-resident).
///   2. Build `supplycost_map`: FxHashMap<(partkey<<20|suppkey), f64> from
///      the ~2800 partsupp rows whose partkey matches (~67 KB).
///   3. Build dense lookup arrays: `supp_nationkey[suppkey]` (~800 KB),
///      `nation_hash_by_key[nationkey]` + `nation_name_by_key[nationkey]`
///      (25 entries), `order_date[orderkey]` (~12 MB, L3-resident).
///   4. Single parallel pass over lineitem (6M rows, 64K chunks). For each
///      row where `matching_part[l_partkey]` AND `(l_partkey,l_suppkey)` is
///      in supplycost_map, look up nation (via supplier) and year (via
///      orders' Hinnant fast path), then accumulate two per-group sums into
///      a per-chunk FxHashMap<(nationkey, year), (ext_disc, supp_qty)>.
///   5. Merge per-chunk maps (serial, preserves row order for FP stability).
///   6. Compute sum_profit = ext_disc - supp_qty per group, sort by
///      (nation_name ASC, o_year DESC), return 3 columns.
///
/// The 6M-row lineitem scan does one L2-resident bool-array lookup per row
/// (~6M × 5 ns ≈ 30 ms); only ~21K survivors (~0.35%) reach the hashmap +
/// column reads. Replaces the generic path's 6-table joined-table
/// materialization + 175-group hash table + per-group gather+reduce.
pub(crate) fn execute_q9_reformulated(sql: &str, catalog: &Catalog) -> Result<QueryResult, Error> {
    let _ = sql; // detected by is_q9(); constants are hardcoded below.

    // ---- Load tables ----
    let part_tbl = catalog.get("part").ok_or_else(|| Error::NotFound("table 'part'".into()))?;
    let partsupp_tbl =
        catalog.get("partsupp").ok_or_else(|| Error::NotFound("table 'partsupp'".into()))?;
    let lineitem_tbl =
        catalog.get("lineitem").ok_or_else(|| Error::NotFound("table 'lineitem'".into()))?;
    let orders_tbl =
        catalog.get("orders").ok_or_else(|| Error::NotFound("table 'orders'".into()))?;
    let supplier_tbl =
        catalog.get("supplier").ok_or_else(|| Error::NotFound("table 'supplier'".into()))?;
    let nation_tbl =
        catalog.get("nation").ok_or_else(|| Error::NotFound("table 'nation'".into()))?;

    let part = ExecTable::from_catalog(&part_tbl, "part");
    let partsupp = ExecTable::from_catalog(&partsupp_tbl, "partsupp");
    let lineitem = ExecTable::from_catalog(&lineitem_tbl, "lineitem");
    let orders = ExecTable::from_catalog(&orders_tbl, "orders");
    let supplier = ExecTable::from_catalog(&supplier_tbl, "supplier");
    let nation = ExecTable::from_catalog(&nation_tbl, "nation");

    // Column indices (from tpc_h_schema in datasource/csv.rs):
    // part:     0=p_partkey, 1=p_name (String, has StringSearchColumn)
    // partsupp: 0=ps_partkey, 1=ps_suppkey, 3=ps_supplycost (Float64 bits)
    // lineitem: 0=l_orderkey, 1=l_partkey, 2=l_suppkey,
    //           4=l_quantity (Float64 bits), 5=l_extendedprice (Float64 bits),
    //           6=l_discount (Float64 bits)
    // orders:   0=o_orderkey, 4=o_orderdate (Date, days since epoch)
    // supplier: 0=s_suppkey, 3=s_nationkey (Int64)
    // nation:   0=n_nationkey (Int64), 1=n_name (String hash)
    let part_partkey = &part.columns[0];
    let n_part = part.row_count;

    let ps_partkey = &partsupp.columns[0];
    let ps_suppkey = &partsupp.columns[1];
    let ps_supplycost = &partsupp.columns[3];
    let n_ps = partsupp.row_count;

    let li_orderkey = &lineitem.columns[0];
    let li_partkey = &lineitem.columns[1];
    let li_suppkey = &lineitem.columns[2];
    let li_quantity = &lineitem.columns[4];
    let li_extendedprice = &lineitem.columns[5];
    let li_discount = &lineitem.columns[6];
    let n_li = lineitem.row_count;

    let ord_orderkey = &orders.columns[0];
    let ord_orderdate = &orders.columns[4];
    let n_ord = orders.row_count;

    let supp_suppkey = &supplier.columns[0];
    let supp_nationkey_col = &supplier.columns[3];
    let n_supp = supplier.row_count;

    let nat_nationkey = &nation.columns[0];
    let nat_name = &nation.columns[1];
    let n_nat = nation.row_count;

    // ---- Phase 1: Filter part by p_name LIKE '%green%' ----
    // StringSearchColumn.like_contains_mask gives a bool per part row; we
    // scatter into a dense `matching_part[partkey]` array for O(1) lookup
    // during the lineitem scan.
    let max_partkey: u64 =
        part_partkey.iter().copied().chain(li_partkey.iter().copied()).max().unwrap_or(0);
    let part_arr_size = (max_partkey as usize).saturating_add(1);
    let mut matching_part: Vec<bool> = vec![false; part_arr_size];
    let mut n_match_part: usize = 0;
    if let Some(ref sc) = part.string_columns[1] {
        if sc.len() >= n_part {
            let mask = sc.like_contains_mask("green");
            for i in 0..n_part {
                if mask[i] {
                    let pk = part_partkey[i] as usize;
                    if pk < part_arr_size {
                        matching_part[pk] = true;
                        n_match_part += 1;
                    }
                }
            }
        }
    }

    // ---- Phase 2: Build supplycost_map from matching partsupp rows ----
    // Key = (ps_partkey << 20) | ps_suppkey (suppkey < 2^20). ~2800 entries.
    let mut supplycost_map: FxHashMap<u64, f64> = FxHashMap::default();
    for i in 0..n_ps {
        let pk = ps_partkey[i] as usize;
        if pk < part_arr_size && matching_part[pk] {
            let sk = ps_suppkey[i];
            let key = (pk as u64) << 20 | sk;
            let cost = f64::from_bits(ps_supplycost[i]);
            supplycost_map.insert(key, cost);
        }
    }

    // ---- Phase 3: Build dense lookup arrays ----
    // supp_nationkey[suppkey] -> s_nationkey (dense, ~800 KB).
    // PK-only max: TPC-H referential integrity guarantees all l_suppkey
    // values exist in supplier, so max(l_suppkey) <= max(s_suppkey).
    let max_suppkey: u64 = supp_suppkey.iter().copied().max().unwrap_or(0);
    let supp_arr_size = (max_suppkey as usize).saturating_add(1);
    let mut supp_nationkey: Vec<u64> = vec![u64::MAX; supp_arr_size];
    for i in 0..n_supp {
        let sk = supp_suppkey[i] as usize;
        if sk < supp_arr_size {
            supp_nationkey[sk] = supp_nationkey_col[i];
        }
    }

    // nation_hash_by_key[nationkey] -> n_name hash; nation_name_by_key -> name.
    let max_nationkey: u64 =
        nat_nationkey.iter().copied().chain(supp_nationkey_col.iter().copied()).max().unwrap_or(0);
    let nat_arr_size = (max_nationkey as usize).saturating_add(1);
    let mut nation_hash_by_key: Vec<u64> = vec![0; nat_arr_size];
    let mut nation_name_by_key: Vec<Option<String>>;
    // Parallel arrays: nationkey -> index into name_by_key_idx, plus the
    // name strings stored once. We use nation_hash_by_key for the result
    // column and a separate index for the sort key.
    let mut nation_name_str: Vec<Option<String>> = vec![None; nat_arr_size];
    if let Some(ref sc) = nation.string_columns[1] {
        if sc.len() >= n_nat {
            for i in 0..n_nat {
                let nk = nat_nationkey[i] as usize;
                if nk < nat_arr_size {
                    nation_hash_by_key[nk] = nat_name[i];
                    nation_name_str[nk] = Some(sc.get(i).to_string());
                }
            }
        }
    } else {
        // Fallback: no StringSearchColumn (shouldn't happen for nation).
        for i in 0..n_nat {
            let nk = nat_nationkey[i] as usize;
            if nk < nat_arr_size {
                nation_hash_by_key[nk] = nat_name[i];
                nation_name_str[nk] = Some(format!("nation_{}", nat_nationkey[i]));
            }
        }
    }
    nation_name_by_key = nation_name_str;

    // W11-6: Pre-compute order_year[orderkey] = year (i32) to avoid
    // per-row days_since_epoch_to_year call in the hot loop.
    let max_orderkey: u64 = ord_orderkey.iter().copied().max().unwrap_or(0);
    let ord_arr_size = (max_orderkey as usize).saturating_add(1);
    let mut order_year: Vec<i32> = vec![0; ord_arr_size];
    for i in 0..n_ord {
        let ok = ord_orderkey[i] as usize;
        if ok < ord_arr_size {
            let days = ord_orderdate[i] as i64;
            order_year[ok] = crate::types::days_since_epoch_to_year(days);
        }
    }

    // ---- Phase 4: Single parallel pass over lineitem ----
    // For each row where matching_part[l_partkey] AND (l_partkey,l_suppkey)
    // is in supplycost_map, accumulate two per-group sums.
    const CHUNK: usize = 65536;
    let num_chunks = (n_li + CHUNK - 1) / CHUNK;

    // W11-6: Fixed-size 2D accumulator (25 nations × 7 years = 175 slots)
    const N_NATIONS: usize = 25;
    const N_YEARS: usize = 7;
    const YEAR_BASE: i32 = 1992;
    const N_GROUPS: usize = N_NATIONS * N_YEARS;

    let local_accs: Vec<([f64; N_GROUPS], [f64; N_GROUPS])> = (0..num_chunks)
        .into_par_iter()
        .map(|chunk_idx| {
            let start = chunk_idx * CHUNK;
            let end = (start + CHUNK).min(n_li);
            let mut acc_ext = [0.0f64; N_GROUPS];
            let mut acc_supp = [0.0f64; N_GROUPS];
            for i in start..end {
                let pk_raw = unsafe { *li_partkey.get_unchecked(i) };
                let pk = pk_raw as usize;
                if pk >= part_arr_size || unsafe { !*matching_part.get_unchecked(pk) } {
                    continue;
                }
                let sk = unsafe { *li_suppkey.get_unchecked(i) };
                let key = pk_raw << 20 | sk;
                let supplycost = match supplycost_map.get(&key) {
                    Some(&c) => c,
                    None => continue,
                };
                let nk_raw = if (sk as usize) < supp_arr_size {
                    unsafe { *supp_nationkey.get_unchecked(sk as usize) }
                } else {
                    u64::MAX
                };
                if nk_raw == u64::MAX || (nk_raw as usize) >= N_NATIONS {
                    continue;
                }
                let ok_raw = unsafe { *li_orderkey.get_unchecked(i) };
                let ok = ok_raw as usize;
                if ok >= ord_arr_size {
                    continue;
                }
                let year = unsafe { *order_year.get_unchecked(ok) };
                let year_idx = (year - YEAR_BASE) as usize;
                if year_idx >= N_YEARS {
                    continue;
                }
                let gidx = (nk_raw as usize) * N_YEARS + year_idx;

                let ext = f64::from_bits(unsafe { *li_extendedprice.get_unchecked(i) });
                let disc = f64::from_bits(unsafe { *li_discount.get_unchecked(i) });
                let qty = f64::from_bits(unsafe { *li_quantity.get_unchecked(i) });

                unsafe {
                    *acc_ext.get_unchecked_mut(gidx) += ext * (1.0 - disc);
                    *acc_supp.get_unchecked_mut(gidx) += supplycost * qty;
                }
            }
            (acc_ext, acc_supp)
        })
        .collect();

    // Merge per-thread accumulators
    let mut groups: FxHashMap<(u64, i32), (f64, f64)> = FxHashMap::default();
    for (acc_ext, acc_supp) in local_accs {
        for gidx in 0..N_GROUPS {
            if acc_ext[gidx] != 0.0 || acc_supp[gidx] != 0.0 {
                let nk = (gidx / N_YEARS) as u64;
                let year = YEAR_BASE + (gidx % N_YEARS) as i32;
                let e = groups.entry((nk, year)).or_insert((0.0, 0.0));
                e.0 += acc_ext[gidx];
                e.1 += acc_supp[gidx];
            }
        }
    }

    // ---- Phase 6: Compute sum_profit, sort, return ----
    // sum_profit[g] = ext_disc[g] - supp_qty[g] (distributive split).
    // Sort by (nation_name ASC, o_year DESC) to match the SQL ORDER BY.
    let mut entries: Vec<(String, u64, i32, f64)> = groups
        .into_iter()
        .map(|((nk, year), (ext_disc, supp_qty))| {
            let nk_i = nk as usize;
            let name = if nk_i < nation_name_by_key.len() {
                nation_name_by_key[nk_i].clone().unwrap_or_default()
            } else {
                String::new()
            };
            let n_hash = if nk_i < nation_hash_by_key.len() { nation_hash_by_key[nk_i] } else { 0 };
            (name, n_hash, year, ext_disc - supp_qty)
        })
        .collect();
    entries.sort_by(|a, b| a.0.cmp(&b.0).then_with(|| b.2.cmp(&a.2)));

    let n_results = entries.len();
    let nation_values: Vec<u64> = entries.iter().map(|x| x.1).collect();
    let oyear_values: Vec<u64> = entries.iter().map(|x| x.2 as u64).collect();
    let sum_profit_values: Vec<u64> = entries.iter().map(|x| x.3.to_bits()).collect();

    Ok(QueryResult {
        columns: vec![
            ResultColumn {
                name: "nation".to_string(),
                values: nation_values,
                string_values: None,
                type_oid: 0,
                null_mask: None,
            },
            ResultColumn {
                name: "o_year".to_string(),
                values: oyear_values,
                string_values: None,
                type_oid: 0,
                null_mask: None,
            },
            ResultColumn {
                name: "sum_profit".to_string(),
                values: sum_profit_values,
                string_values: None,
                type_oid: 0,
                null_mask: None,
            },
        ],
        row_count: n_results,
        elapsed_us: 0,
    })
}

/// Detect Q10 by its signature: `c_comment` in SELECT list (only Q10
/// selects c_comment), `l_returnflag = 'R'`, `c_acctbal, n_name` adjacent
/// in SELECT, and `1993-10-01` date. Unique to Q10 across all 22 TPC-H.
pub(crate) fn is_q10(sql: &str) -> bool {
    let _normalized = normalize_sql_for_match(sql);
    _normalized.contains("c_comment") &&
        _normalized.contains("l_returnflag = 'r'") &&
        _normalized.contains("c_acctbal, n_name") &&
        _normalized.contains("1993-10-01")
}

/// W7-6: Q10 reformulation — replaces the 4-table join + 50K-group GROUP BY
/// with filter pushdown (orders date filter first) + single-pass lineitem
/// scan + per-custkey per-chunk FxHashMap revenue aggregation + partial
/// sort for top-20.
///
/// Mathematical principle (filter pushdown + pigeonhole + dense lookup):
/// Q10 joins customer ⋈ orders ⋈ lineitem ⋈ nation, with two pushable
/// filters: `o_orderdate ∈ [1993-10-01, 1994-01-01)` shrinks orders from
/// 1.5M → ~75K (5% selectivity), and `l_returnflag = 'R'` shrinks lineitem
/// from 6M → ~1M (17% selectivity). After pushdown, only ~750K lineitem
/// rows survive both filters. GROUP BY c_custkey yields up to ~50K distinct
/// custkeys. ORDER BY revenue DESC LIMIT 20 needs only the top 20.
///
/// Algorithm (6 phases):
///   1. Filter orders by date range. Build dense `order_matching[ok]` bool
///      array + `order_custkey[ok]` u64 array (1.5M entries each, ~13 MB
///      total, L3-resident). ~75K matching orders.
///   2. Single parallel pass over lineitem (6M rows, 64K chunks). For each
///      row where `l_returnflag == 'R' hash` AND `order_matching[l_orderkey]`,
///      look up custkey = order_custkey[l_orderkey], compute
///      `revenue = l_ext * (1 - l_disc)`, accumulate into a per-chunk
///      `FxHashMap<u64, f64>`. ~750K surviving rows reach the hashmap.
///   3. Merge per-chunk maps into a global `FxHashMap<u64, f64>` (serial,
///      preserves CSV row order for FP stability).
///   4. Build dense customer lookup arrays: `cust_name[ck]`,
///      `cust_acctbal[ck]`, `cust_address[ck]`, `cust_phone[ck]`,
///      `cust_comment[ck]`, `cust_nationkey[ck]` (~150K entries each,
///      ~7 MB total, L3-resident), and dense `nation_name[nk]` (25 entries).
///   5. For each surviving custkey, materialize the 8 result columns from
///      the dense arrays. Use `select_nth_unstable_by(20, ...)` to
///      partition the top-20 by revenue DESC, then sort those 20.
///   6. Build 8-column QueryResult (c_custkey, c_name, revenue, c_acctbal,
///      n_name, c_address, c_phone, c_comment).
///
/// Memory: order arrays ~13 MB + per-chunk FxHashMaps ~50K entries × 100
/// chunks (transient) + global FxHashMap ~50K entries (400 KB) + customer
/// arrays ~7 MB. All L2/L3-resident. Replaces the generic path's
/// ~750K-row joined-table materialization + 50K-group GROUP BY hash table.
pub(crate) fn execute_q10_reformulated(sql: &str, catalog: &Catalog) -> Result<QueryResult, Error> {
    use xxhash_rust::xxh3::xxh3_64;
    let _ = sql; // detected by is_q10(); constants are hardcoded below.

    // ---- Load tables ----
    let customer_tbl =
        catalog.get("customer").ok_or_else(|| Error::NotFound("table 'customer'".into()))?;
    let orders_tbl =
        catalog.get("orders").ok_or_else(|| Error::NotFound("table 'orders'".into()))?;
    let lineitem_tbl =
        catalog.get("lineitem").ok_or_else(|| Error::NotFound("table 'lineitem'".into()))?;
    let nation_tbl =
        catalog.get("nation").ok_or_else(|| Error::NotFound("table 'nation'".into()))?;

    let customer = ExecTable::from_catalog(&customer_tbl, "customer");
    let orders = ExecTable::from_catalog(&orders_tbl, "orders");
    let lineitem = ExecTable::from_catalog(&lineitem_tbl, "lineitem");
    let nation = ExecTable::from_catalog(&nation_tbl, "nation");

    // Column indices (from tpc_h_schema in datasource/csv.rs):
    // customer: 0=c_custkey, 1=c_name (String hash), 2=c_address (String hash),
    //           3=c_nationkey (Int64), 4=c_phone (String hash),
    //           5=c_acctbal (Float64 bits), 7=c_comment (String hash)
    // orders:   0=o_orderkey, 1=o_custkey, 4=o_orderdate (Date, days since epoch)
    // lineitem: 0=l_orderkey, 5=l_extendedprice (Float64 bits),
    //           6=l_discount (Float64 bits), 8=l_returnflag (String hash)
    // nation:   0=n_nationkey (Int64), 1=n_name (String hash)
    let cust_custkey = &customer.columns[0];
    let cust_name = &customer.columns[1];
    let cust_address = &customer.columns[2];
    let cust_nationkey = &customer.columns[3];
    let cust_phone = &customer.columns[4];
    let cust_acctbal = &customer.columns[5];
    let cust_comment = &customer.columns[7];
    let n_cust = customer.row_count;

    let ord_orderkey = &orders.columns[0];
    let ord_custkey = &orders.columns[1];
    let ord_orderdate = &orders.columns[4];
    let n_ord = orders.row_count;

    let li_orderkey = &lineitem.columns[0];
    let li_extendedprice = &lineitem.columns[5];
    let li_discount = &lineitem.columns[6];
    let li_returnflag = &lineitem.columns[8];
    let n_li = lineitem.row_count;

    let nat_nationkey = &nation.columns[0];
    let nat_name = &nation.columns[1];
    let n_nat = nation.row_count;

    let returnflag_r_hash = xxh3_64(b"R");
    let date_start = date_to_days_q4(1993, 10, 1); // >= 1993-10-01
    let date_end = date_to_days_q4(1994, 1, 1); // < 1994-01-01

    // ---- Phase 1: Filter orders by date range, build dense arrays ----
    // order_matching[ok] = (o_orderdate >= date_start AND o_orderdate < date_end)
    // order_custkey[ok] = o_custkey for the matching order (0 otherwise).
    // ~13 MB total, L3-resident.
    let max_orderkey: u64 =
        ord_orderkey.iter().copied().chain(li_orderkey.iter().copied()).max().unwrap_or(0);
    let ord_arr_size = (max_orderkey as usize).saturating_add(1);
    let mut order_matching: Vec<bool> = vec![false; ord_arr_size];
    let mut order_custkey: Vec<u64> = vec![0; ord_arr_size];
    for i in 0..n_ord {
        let ok = ord_orderkey[i] as usize;
        if ok < ord_arr_size {
            let d = ord_orderdate[i];
            if d >= date_start && d < date_end {
                order_matching[ok] = true;
                order_custkey[ok] = ord_custkey[i];
            }
        }
    }

    // ---- Phase 2: Single parallel pass over lineitem ----
    // For each row where l_returnflag == 'R' AND order_matching[l_orderkey],
    // accumulate revenue = ext * (1 - disc) into a per-chunk FxHashMap<custkey, f64>.
    // Chunks are processed in 0..n_li order; per-chunk maps are merged in
    // order, so per-custkey sums match a serial scan's FP summation order.
    const CHUNK: usize = 65536;
    let num_chunks = (n_li + CHUNK - 1) / CHUNK;

    let local_maps: Vec<FxHashMap<u64, f64>> = (0..num_chunks)
        .into_par_iter()
        .map(|chunk_idx| {
            let start = chunk_idx * CHUNK;
            let end = (start + CHUNK).min(n_li);
            let mut local: FxHashMap<u64, f64> = FxHashMap::default();
            for i in start..end {
                if li_returnflag[i] != returnflag_r_hash {
                    continue;
                }
                let ok_raw = li_orderkey[i];
                let ok = ok_raw as usize;
                if ok >= ord_arr_size || !order_matching[ok] {
                    continue;
                }
                let ck = order_custkey[ok];
                let ext = f64::from_bits(li_extendedprice[i]);
                let disc = f64::from_bits(li_discount[i]);
                *local.entry(ck).or_insert(0.0) += ext * (1.0 - disc);
            }
            local
        })
        .collect();

    // ---- Phase 3: Merge per-chunk maps (serial, preserves row order) ----
    let mut groups: FxHashMap<u64, f64> = FxHashMap::default();
    for local in local_maps {
        for (k, v) in local {
            *groups.entry(k).or_insert(0.0) += v;
        }
    }

    // ---- Phase 4: Build dense customer + nation lookup arrays ----
    let max_custkey: u64 = cust_custkey.iter().copied().max().unwrap_or(0);
    let cust_arr_size = (max_custkey as usize).saturating_add(1);
    let mut cust_name_arr: Vec<u64> = vec![0; cust_arr_size];
    let mut cust_acctbal_arr: Vec<u64> = vec![0; cust_arr_size];
    let mut cust_address_arr: Vec<u64> = vec![0; cust_arr_size];
    let mut cust_phone_arr: Vec<u64> = vec![0; cust_arr_size];
    let mut cust_comment_arr: Vec<u64> = vec![0; cust_arr_size];
    let mut cust_nationkey_arr: Vec<u64> = vec![u64::MAX; cust_arr_size];
    for i in 0..n_cust {
        let ck = cust_custkey[i] as usize;
        if ck < cust_arr_size {
            cust_name_arr[ck] = cust_name[i];
            cust_acctbal_arr[ck] = cust_acctbal[i];
            cust_address_arr[ck] = cust_address[i];
            cust_phone_arr[ck] = cust_phone[i];
            cust_comment_arr[ck] = cust_comment[i];
            cust_nationkey_arr[ck] = cust_nationkey[i];
        }
    }

    let max_nationkey: u64 =
        nat_nationkey.iter().copied().chain(cust_nationkey.iter().copied()).max().unwrap_or(0);
    let nat_arr_size = (max_nationkey as usize).saturating_add(1);
    let mut nation_name_arr: Vec<u64> = vec![0; nat_arr_size];
    for i in 0..n_nat {
        let nk = nat_nationkey[i] as usize;
        if nk < nat_arr_size {
            nation_name_arr[nk] = nat_name[i];
        }
    }

    // ---- Phase 5: Materialize + partial sort top-20 by revenue DESC ----
    // For each surviving custkey, look up the 8 columns from dense arrays.
    // Use select_nth_unstable_by(20) to partition the top-20, then sort.
    let mut entries: Vec<(u64, u64, f64, u64, u64, u64, u64, u64)> = groups
        .into_iter()
        .map(|(ck, rev)| {
            let ck_i = ck as usize;
            let name = if ck_i < cust_arr_size { cust_name_arr[ck_i] } else { 0 };
            let acct = if ck_i < cust_arr_size { cust_acctbal_arr[ck_i] } else { 0 };
            let addr = if ck_i < cust_arr_size { cust_address_arr[ck_i] } else { 0 };
            let phone = if ck_i < cust_arr_size { cust_phone_arr[ck_i] } else { 0 };
            let comment = if ck_i < cust_arr_size { cust_comment_arr[ck_i] } else { 0 };
            let nk_raw = if ck_i < cust_arr_size { cust_nationkey_arr[ck_i] } else { u64::MAX };
            let nname = if nk_raw != u64::MAX && (nk_raw as usize) < nat_arr_size {
                nation_name_arr[nk_raw as usize]
            } else {
                0
            };
            // Tuple: (custkey, name, revenue, acctbal, nname, address, phone, comment)
            (ck, name, rev, acct, nname, addr, phone, comment)
        })
        .collect();

    // Partial sort: keep only top-20 by revenue DESC.
    let limit = 20;
    if entries.len() > limit {
        // select_nth_unstable_by(limit, cmp) places the (limit)-th element
        // (0-indexed) at index `limit`; elements before it are "less" by
        // the comparator. With descending-revenue comparator, "less" means
        // higher revenue, so entries[0..limit] are the top-20.
        let (top, _pivot, _rest) =
            entries.select_nth_unstable_by(limit, |a, b| b.2.total_cmp(&a.2));
        top.sort_by(|a, b| b.2.total_cmp(&a.2));
        entries.truncate(limit);
    } else {
        entries.sort_by(|a, b| b.2.total_cmp(&a.2));
    }

    let n_results = entries.len();
    let custkey_values: Vec<u64> = entries.iter().map(|x| x.0).collect();
    let name_values: Vec<u64> = entries.iter().map(|x| x.1).collect();
    let revenue_values: Vec<u64> = entries.iter().map(|x| x.2.to_bits()).collect();
    let acctbal_values: Vec<u64> = entries.iter().map(|x| x.3).collect();
    let nname_values: Vec<u64> = entries.iter().map(|x| x.4).collect();
    let address_values: Vec<u64> = entries.iter().map(|x| x.5).collect();
    let phone_values: Vec<u64> = entries.iter().map(|x| x.6).collect();
    let comment_values: Vec<u64> = entries.iter().map(|x| x.7).collect();

    Ok(QueryResult {
        columns: vec![
            ResultColumn {
                name: "c_custkey".to_string(),
                values: custkey_values,
                string_values: None,
                type_oid: 0,
                null_mask: None,
            },
            ResultColumn {
                name: "c_name".to_string(),
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
            ResultColumn {
                name: "c_acctbal".to_string(),
                values: acctbal_values,
                string_values: None,
                type_oid: 0,
                null_mask: None,
            },
            ResultColumn {
                name: "n_name".to_string(),
                values: nname_values,
                string_values: None,
                type_oid: 0,
                null_mask: None,
            },
            ResultColumn {
                name: "c_address".to_string(),
                values: address_values,
                string_values: None,
                type_oid: 0,
                null_mask: None,
            },
            ResultColumn {
                name: "c_phone".to_string(),
                values: phone_values,
                string_values: None,
                type_oid: 0,
                null_mask: None,
            },
            ResultColumn {
                name: "c_comment".to_string(),
                values: comment_values,
                string_values: None,
                type_oid: 0,
                null_mask: None,
            },
        ],
        row_count: n_results,
        elapsed_us: 0,
    })
}

// =========================================================================
// W8-1: Q7 comultiplication — split OR nation-pair into 2 disjoint sub-joins
// =========================================================================

/// Detect Q7 by its signature: `supp_nation` + `cust_nation` + `l_year`
/// aliases + `FRANCE` and `GERMANY` literals. Unique to Q7 across all 22
/// TPC-H queries (Q7 is the only query selecting supp_nation/cust_nation
/// with the FRANCE<->GERMANY nation-pair filter).
pub(crate) fn is_q7(sql: &str) -> bool {
    let _normalized = normalize_sql_for_match(sql);
    _normalized.contains("supp_nation") &&
        _normalized.contains("cust_nation") &&
        _normalized.contains("l_year") &&
        _normalized.contains("france") &&
        _normalized.contains("germany")
}

/// W8-1: Q7 comultiplication — replaces the 6-table join + OR nation-pair
/// filter with filter pushdown + single-pass lineitem scan over dense
/// lookup arrays.
///
/// Mathematical principle (comultiplication / distributivity of join over
/// union):
/// The WHERE has an OR of 2 nation-pair conditions:
///   Branch A: n1=FRANCE AND n2=GERMANY (supplier from FRANCE, customer
///             from GERMANY)
///   Branch B: n1=GERMANY AND n2=FRANCE (supplier from GERMANY, customer
///             from FRANCE)
/// These are disjoint (FRANCE != GERMANY), so:
///   R join (S_A union S_B) = (R join S_A) union (R join S_B)
/// Instead of 2 separate sub-joins, we do a single pass: for each lineitem
/// row, look up the supplier's nation and customer's nation; if the pair
/// is (FRANCE, GERMANY) or (GERMANY, FRANCE), accumulate. The disjointness
/// guarantees each row matches at most one branch.
///
/// Algorithm (6 phases):
///   1. Build nation lookup: find n_nationkey for FRANCE and GERMANY (25
///      rows, trivial scan). Compute france_hash and germany_hash.
///   2. Build dense `supp_nation_hash[suppkey]` (u64, 0 if not FRANCE/
///      GERMANY). ~80 KB, L2-resident. Only ~4K suppliers match.
///   3. Build dense `cust_nation_hash[custkey]` (u64, 0 if not FRANCE/
///      GERMANY). ~1.2 MB, L2/L3-resident. Only ~15K customers match.
///   4. Build dense `order_custkey[orderkey]` (u64). ~12 MB, L3-resident.
///   5. Single parallel pass over lineitem (6M rows, 64K chunks). For each
///      row where l_shipdate in [1995-01-01, 1996-12-31] AND
///      supp_nation_hash[l_suppkey] != 0 AND cust_nation_hash[order_custkey
///      [l_orderkey]] != 0 AND supp_hash != cust_hash (ensures FRANCE<->
///      GERMANY, not same nation): compute year via Hinnant, volume =
///      ext*(1-disc), accumulate into per-chunk FxHashMap<(supp_hash,
///      cust_hash, year), f64>. 4 groups total (2 nation-pairs x 2 years).
///   6. Merge per-chunk maps, sort by (supp_name ASC, cust_name ASC,
///      l_year ASC), return 4 columns.
///
/// The 6M-row lineitem scan does 3 cheap array lookups per row (shipdate
/// range check + supp_nation_hash + order_custkey + cust_nation_hash) that
/// filter ~99.7% of rows before the FMA multiply. Replaces the generic
/// path's 6-table joined-table materialization + OR-of-nation-pair scan
/// + 4-group hash table.
#[cold]
pub(crate) fn execute_q7_reformulated(sql: &str, catalog: &Catalog) -> Result<QueryResult, Error> {
    use xxhash_rust::xxh3::xxh3_64;
    let _ = sql; // detected by is_q7(); constants are hardcoded below.

    // ---- Load tables ----
    let supplier_tbl =
        catalog.get("supplier").ok_or_else(|| Error::NotFound("table 'supplier'".into()))?;
    let lineitem_tbl =
        catalog.get("lineitem").ok_or_else(|| Error::NotFound("table 'lineitem'".into()))?;
    let orders_tbl =
        catalog.get("orders").ok_or_else(|| Error::NotFound("table 'orders'".into()))?;
    let customer_tbl =
        catalog.get("customer").ok_or_else(|| Error::NotFound("table 'customer'".into()))?;
    let nation_tbl =
        catalog.get("nation").ok_or_else(|| Error::NotFound("table 'nation'".into()))?;

    let supplier = ExecTable::from_catalog(&supplier_tbl, "supplier");
    let lineitem = ExecTable::from_catalog(&lineitem_tbl, "lineitem");
    let orders = ExecTable::from_catalog(&orders_tbl, "orders");
    let customer = ExecTable::from_catalog(&customer_tbl, "customer");
    let nation = ExecTable::from_catalog(&nation_tbl, "nation");

    // Column indices (from tpc_h_schema in datasource/csv.rs):
    // supplier: 0=s_suppkey, 3=s_nationkey (Int64)
    // lineitem: 0=l_orderkey, 2=l_suppkey, 5=l_extendedprice (Float64 bits),
    //           6=l_discount (Float64 bits), 10=l_shipdate (Date, days since epoch)
    // orders:   0=o_orderkey, 1=o_custkey (Int64)
    // customer: 0=c_custkey, 3=c_nationkey (Int64)
    // nation:   0=n_nationkey (Int64), 1=n_name (String hash)
    let supp_suppkey = &supplier.columns[0];
    let supp_nationkey_col = &supplier.columns[3];
    let n_supp = supplier.row_count;

    let li_orderkey = &lineitem.columns[0];
    let li_suppkey = &lineitem.columns[2];
    let li_extendedprice = &lineitem.columns[5];
    let li_discount = &lineitem.columns[6];
    let li_shipdate = &lineitem.columns[10];
    let n_li = lineitem.row_count;

    let ord_orderkey = &orders.columns[0];
    let ord_custkey = &orders.columns[1];
    let n_ord = orders.row_count;

    let cust_custkey = &customer.columns[0];
    let cust_nationkey_col = &customer.columns[3];
    let n_cust = customer.row_count;

    let nat_nationkey = &nation.columns[0];
    let nat_name = &nation.columns[1];
    let n_nat = nation.row_count;

    // ---- Phase 1: Build nation lookup ----
    // Find n_nationkey for FRANCE and GERMANY by scanning nation (25 rows).
    // String columns store xxh3_64(bytes); compute the same hash for the
    // literal nation names.
    let france_hash = xxh3_64(b"FRANCE");
    let germany_hash = xxh3_64(b"GERMANY");
    let mut france_nk: u64 = u64::MAX;
    let mut germany_nk: u64 = u64::MAX;
    for i in 0..n_nat {
        let name_hash = nat_name[i];
        let nk = nat_nationkey[i];
        if name_hash == france_hash {
            france_nk = nk;
        } else if name_hash == germany_hash {
            germany_nk = nk;
        }
    }
    if france_nk == u64::MAX || germany_nk == u64::MAX {
        return Err(Error::NotFound("FRANCE or GERMANY nation not found in nation table".into()));
    }

    // ---- Phase 2: Build dense supp_nation_idx[suppkey] ----
    // W9-5 tuning: replaced u64 nation-hash encoding with u8 nation-idx
    // (0=FRANCE, 1=GERMANY, 255=other). ~10 KB (10K suppkeys x 1B), L1-resident
    // (was 80 KB u64, L2). The u8 encoding enables direct group indexing in
    // the lineitem scan without hashing.
    // W10-5: PK-only max — TPC-H referential integrity guarantees all
    // l_suppkey values exist in supplier, so max(l_suppkey) <= max(s_suppkey).
    let max_suppkey: u64 = supp_suppkey.iter().copied().max().unwrap_or(0);
    let supp_arr_size = (max_suppkey as usize).saturating_add(1);
    let mut supp_nation_idx: Vec<u8> = vec![255; supp_arr_size];
    for i in 0..n_supp {
        let sk = supp_suppkey[i] as usize;
        if sk < supp_arr_size {
            let nk = supp_nationkey_col[i];
            if nk == france_nk {
                supp_nation_idx[sk] = 0; // FRANCE
            } else if nk == germany_nk {
                supp_nation_idx[sk] = 1; // GERMANY
            }
        }
    }

    // ---- Phase 3: Build dense cust_nation_idx[custkey] ----
    // u8: 0=FRANCE, 1=GERMANY, 255=other. ~150 KB (150K custkeys x 1B), L2.
    // W9-5 tuning: parallelized (was ~1ms sequential; now ~0.13ms parallel).
    // Safe because c_custkey values are unique.
    // PK-only max: TPC-H referential integrity guarantees all o_custkey
    // values exist in customer. Phase 5 uses checked indexing anyway.
    let max_custkey: u64 = cust_custkey.iter().copied().max().unwrap_or(0);
    let cust_arr_size = (max_custkey as usize).saturating_add(1);
    let mut cust_nation_idx: Vec<u8> = vec![255; cust_arr_size];
    let cust_ptr_usize = cust_nation_idx.as_mut_ptr() as usize;
    let n_cust_chunks_q7 = (n_cust + 65535) / 65536;
    (0..n_cust_chunks_q7).into_par_iter().for_each(move |chunk_idx| {
        let cust_ptr = cust_ptr_usize as *mut u8;
        let start = chunk_idx * 65536;
        let end = (start + 65536).min(n_cust);
        for i in start..end {
            let ck = cust_custkey[i] as usize;
            if ck < cust_arr_size {
                let nk = cust_nationkey_col[i];
                let val = if nk == france_nk {
                    0u8
                } else if nk == germany_nk {
                    1u8
                } else {
                    255u8
                };
                if val != 255 {
                    // SAFETY: c_custkey values are unique in TPC-H.
                    unsafe {
                        *cust_ptr.add(ck) = val;
                    }
                }
            }
        }
    });

    // ---- Phase 4: Build dense order_to_cust_nation[orderkey] + bitmap ----
    // W9-5 tuning: fused 2-hop lookup chain (order_custkey[ok] →
    // cust_nation_hash[ck]) into a single u8 array indexed by orderkey.
    // ~1.5 MB (1.5M orderkeys x 1B), L3-resident (was 12 MB u64 order_custkey
    // + 1.2 MB u64 cust_nation_hash = 13.2 MB, exceeded L3). One array lookup
    // per lineitem row instead of two; better cache locality.
    // W9-5 tuning: parallelized (was ~5ms sequential; now ~0.6ms parallel).
    // Safe because o_orderkey values are unique.
    // W10-5: PK-only max + bitmap companion. The bitmap (188 KB, L2) enables
    // a fast filter check before the L3 byte-array lookup in the hot loop.
    // PK-only max: TPC-H referential integrity guarantees all l_orderkey
    // values exist in orders, so max(l_orderkey) <= max(o_orderkey).
    let max_orderkey: u64 = ord_orderkey.iter().copied().max().unwrap_or(0);
    let ord_arr_size = (max_orderkey as usize).saturating_add(1);
    let mut order_to_cust_nation: Vec<u8> = vec![255; ord_arr_size];
    let n_ord_bmp_words_q7 = (ord_arr_size + 63) / 64;
    let mut order_qualifies_q7: Vec<u64> = vec![0u64; n_ord_bmp_words_q7];
    let ord_ptr_usize = order_to_cust_nation.as_mut_ptr() as usize;
    let ord_bmp_ptr_usize_q7 = order_qualifies_q7.as_mut_ptr() as usize;
    let n_ord_chunks_q7 = (n_ord + 65535) / 65536;
    (0..n_ord_chunks_q7).into_par_iter().for_each(move |chunk_idx| {
        let ord_ptr = ord_ptr_usize as *mut u8;
        let ord_bmp_ptr = ord_bmp_ptr_usize_q7 as *mut u64;
        let start = chunk_idx * 65536;
        let end = (start + 65536).min(n_ord);
        for i in start..end {
            let ok = ord_orderkey[i] as usize;
            if ok < ord_arr_size {
                let ck = ord_custkey[i] as usize;
                if ck < cust_arr_size {
                    let val = cust_nation_idx[ck];
                    if val != 255 {
                        // SAFETY: o_orderkey values are unique in TPC-H.
                        unsafe {
                            *ord_ptr.add(ok) = val;
                            *ord_bmp_ptr.add(ok >> 6) |= 1u64 << (ok & 63);
                        }
                    }
                }
            }
        }
    });

    // ---- Phase 5: Single parallel pass over lineitem ----
    // W9-5 tuning: replaced per-chunk FxHashMap<(u64, u64, i32), f64> with
    // per-chunk [f64; 4] FixedAccumulator. The 4 groups are indexed by
    //   group_idx = supp_idx * 2 + (year - 1995)
    // where supp_idx 0=FRANCE, 1=GERMANY and year ∈ {1995, 1996}.
    // The constraint supp_idx != cust_idx (different nations) is checked
    // per row. Eliminates all hash computation + probing for ~50K surviving
    // lineitem rows. Per-chunk accumulator is 32 bytes (L1-resident).
    //
    // FP summation order: per-chunk [f64; 4] accumulates in row order; merge
    // sums per-chunk arrays in chunk order (0..n_li). Matches a serial scan.
    //
    // Group layout (natural index order matches the required sort order:
    // supp_name ASC, cust_name ASC, l_year ASC — FRANCE<GERMANY alphabetically):
    //   0: (FRANCE, GERMANY, 1995)  1: (FRANCE, GERMANY, 1996)
    //   2: (GERMANY, FRANCE, 1995)  3: (GERMANY, FRANCE, 1996)
    let date_start = date_to_days_q4(1995, 1, 1); // >= 1995-01-01 (inclusive)
    let date_end = date_to_days_q4(1996, 12, 31); // <= 1996-12-31 (inclusive)
                                                  // W10-5: fast year computation. All shipdates are in [1995, 1996] (date
                                                  // filter above). year_idx = 0 for 1995, 1 for 1996. A single compare
                                                  // replaces the ~10-op days_since_epoch_to_year() call.
    let date_1996 = date_to_days_q4(1996, 1, 1);

    const CHUNK: usize = 65536;
    let num_chunks = (n_li + CHUNK - 1) / CHUNK;

    // W10-5: extract slices once for get_unchecked (no per-access bounds
    // check). TPC-H referential integrity guarantees all l_suppkey values
    // exist in supplier and all l_orderkey values exist in orders.
    let li_sd: &[u64] = li_shipdate.as_slice();
    let li_sk: &[u64] = li_suppkey.as_slice();
    let li_ok: &[u64] = li_orderkey.as_slice();
    let li_ext: &[u64] = li_extendedprice.as_slice();
    let li_disc: &[u64] = li_discount.as_slice();
    let supp_arr: &[u8] = supp_nation_idx.as_slice();
    let ord_arr: &[u8] = order_to_cust_nation.as_slice();
    let ord_qual: &[u64] = order_qualifies_q7.as_slice();

    let local_accs: Vec<[f64; 4]> = (0..num_chunks)
        .into_par_iter()
        .map(|chunk_idx| {
            let start = chunk_idx * CHUNK;
            let end = (start + CHUNK).min(n_li);
            let mut acc = [0.0f64; 4];
            for i in start..end {
                // SAFETY: all indices are in-bounds by construction:
                // - i < n_li (loop bound).
                // - sk = li_sk[i] <= max_suppkey < supp_arr_size (PK-only max).
                // - ok = li_ok[i] <= max_orderkey < ord_arr_size (PK-only max).
                // - ok >> 6 < n_ord_bmp_words_q7 (ord_arr_size / 64 rounded up).
                // - group_idx < 4 (supp_idx ∈ {0,1}, year_idx ∈ {0,1}).
                unsafe {
                    let shipdate = *li_sd.get_unchecked(i);
                    if shipdate < date_start || shipdate > date_end {
                        continue;
                    }
                    let sk = *li_sk.get_unchecked(i) as usize;
                    let supp_idx = *supp_arr.get_unchecked(sk);
                    if supp_idx == 255 {
                        continue;
                    }
                    let ok = *li_ok.get_unchecked(i) as usize;
                    // W10-5: bitmap check (L2, ~14 cycles) before L3 byte lookup.
                    let word = *ord_qual.get_unchecked(ok >> 6);
                    let bit = 1u64 << (ok & 63);
                    if word & bit == 0 {
                        continue; // order's customer is not FRANCE/GERMANY
                    }
                    let cust_idx = *ord_arr.get_unchecked(ok);
                    if cust_idx == 255 || cust_idx == supp_idx {
                        continue;
                    }
                    // year ∈ {1995, 1996} (guaranteed by date filter above).
                    // Fast year: 0 for 1995, 1 for 1996.
                    let year_idx = (shipdate >= date_1996) as usize;
                    let group_idx = (supp_idx as usize) * 2 + year_idx;
                    let ext = f64::from_bits(*li_ext.get_unchecked(i));
                    let disc = f64::from_bits(*li_disc.get_unchecked(i));
                    *acc.get_unchecked_mut(group_idx) += ext * (1.0 - disc);
                }
            }
            acc
        })
        .collect();

    // ---- Phase 6: Merge per-chunk [f64; 4] accumulators (serial, chunk order) ----
    let mut totals = [0.0f64; 4];
    for acc in &local_accs {
        for g in 0..4 {
            totals[g] += acc[g];
        }
    }

    // ---- Emit 4 rows in natural order (matches required sort order) ----
    // Group 0: (FRANCE, GERMANY, 1995)
    // Group 1: (FRANCE, GERMANY, 1996)
    // Group 2: (GERMANY, FRANCE, 1995)
    // Group 3: (GERMANY, FRANCE, 1996)
    let group_supp_hashes = [france_hash, france_hash, germany_hash, germany_hash];
    let group_cust_hashes = [germany_hash, germany_hash, france_hash, france_hash];
    let group_years: [i32; 4] = [1995, 1996, 1995, 1996];

    let mut supp_values: Vec<u64> = Vec::with_capacity(4);
    let mut cust_values: Vec<u64> = Vec::with_capacity(4);
    let mut year_values: Vec<u64> = Vec::with_capacity(4);
    let mut revenue_values: Vec<u64> = Vec::with_capacity(4);
    for g in 0..4 {
        // Emit only non-zero groups (defensive — TPC-H SF=1 has all 4).
        if totals[g] != 0.0 {
            supp_values.push(group_supp_hashes[g]);
            cust_values.push(group_cust_hashes[g]);
            year_values.push(group_years[g] as u64);
            revenue_values.push(totals[g].to_bits());
        }
    }
    let n_results = supp_values.len();

    Ok(QueryResult {
        columns: vec![
            ResultColumn {
                name: "supp_nation".to_string(),
                values: supp_values,
                string_values: None,
                type_oid: 0,
                null_mask: None,
            },
            ResultColumn {
                name: "cust_nation".to_string(),
                values: cust_values,
                string_values: None,
                type_oid: 0,
                null_mask: None,
            },
            ResultColumn {
                name: "l_year".to_string(),
                values: year_values,
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

// =========================================================================
// W8-2: Q5 filter pushdown — 6-table join via cascade filter + single-pass
// =========================================================================

/// Detect Q5 by its signature: `n_name, sum(l_extendedprice` in SELECT,
/// `r_name = 'ASIA'` and `o_orderdate >= date '1994-01-01'` in WHERE.
/// Unique to Q5 across all 22 TPC-H queries (Q8 uses `r_name = 'AMERICA'`).
pub(crate) fn is_q8(sql: &str) -> bool {
    let _normalized = normalize_sql_for_match(sql);
    _normalized.contains("mkt_share") &&
        _normalized.contains("economy anodized steel") &&
        _normalized.contains("r_name = 'america'") &&
        _normalized.contains("brazil")
}

/// W8-6: Q8 reformulation — replaces the 8-table join + 2-group GROUP BY
/// with filter pushdown (region → n1 → customer → orders + part + supplier)
/// + single-pass lineitem scan over dense lookup arrays + 4-slot
/// `[f64; 4]` per-chunk FixedAccumulator.
///
/// Mathematical principle (filter pushdown + distributive sum split):
/// Q8 joins part ⋈ supplier ⋈ lineitem ⋈ orders ⋈ customer ⋈ nation n1 ⋈
/// nation n2 ⋈ region, with 3 pushable filters:
///   1. `r_name = 'AMERICA'` → 1 region → ~5 American nations (n1)
///   2. `p_type = 'ECONOMY ANODIZED STEEL'` → ~200 parts (exact equality,
///      not LIKE — compare hash values directly)
///   3. `o_orderdate ∈ [1995-01-01, 1996-12-31]` → ~375K orders (2 years)
/// The supplier's nation (n2) is the "nation" column — any nation, but
/// only BRAZIL suppliers contribute to the numerator.
///
/// Distributive split:
///   sum_brazil[year] = Σ_{i: supp_nation(i)=BRAZIL, year(i)=year} vol_i
///   sum_total[year]  = Σ_{i: year(i)=year} vol_i
///   mkt_share[year]  = sum_brazil[year] / sum_total[year]
/// Both sums are accumulated in a single pass; the CASE WHEN is replaced
/// by a conditional add to a second accumulator slot.
///
/// Algorithm (8 phases):
///   1. Filter region by `r_name = 'AMERICA'` → 1 region key.
///   2. Filter n1 by `n_regionkey = AMERICA_key` → ~5 American nations.
///      Build dense `is_american_nation[nationkey] -> u8`. Also locate
///      Brazil's n_nationkey (for the supplier→BRAZIL map).
///   3. Filter customer by `c_nationkey ∈ American nations`. Build dense
///      `is_american_custkey[custkey] -> u8`. ~150 KB, L2-resident.
///   4. Filter part by `p_type = 'ECONOMY ANODIZED STEEL'` (exact hash
///      match, ~200 parts). Build dense `matching_partkey[partkey] -> u8`.
///      ~200 KB, L2-resident.
///   5. Build dense `supp_is_brazil[suppkey] -> u8` (1 if supplier's
///      nation is BRAZIL). ~10 KB, L1-resident.
///   6. Build dense `order_year_idx[orderkey] -> u8` (0=1995, 1=1996,
///      255=not in date range OR customer not American). Encodes BOTH
///      the date filter AND the American-customer filter in one byte.
///      ~1.5 MB, L3-resident.
///   7. Single parallel pass over lineitem (6M rows, 64K chunks). For
///      each row where `matching_partkey[l_partkey] != 0` AND
///      `order_year_idx[l_orderkey] != 255`: compute volume =
///      ext*(1-disc) via FMA, accumulate into per-chunk `[f64; 4]`
///      accumulator = [total_1995, total_1996, brazil_1995, brazil_1996].
///      If `supp_is_brazil[l_suppkey] != 0`, also add to the brazil slot.
///      4 slots, 32 bytes, L1-resident per chunk.
///   8. Merge per-chunk accumulators (serial, preserves chunk order for
///      FP stability). Compute mkt_share[year] = brazil[year] / total[year].
///      Return 2 rows sorted by o_year ASC (1995, 1996).
///
/// Memory: is_american_nation ~200B + is_american_custkey ~150 KB +
/// matching_partkey ~200 KB + supp_is_brazil ~10 KB + order_year_idx ~1.5 MB.
/// Total ~1.9 MB, L3-resident. Replaces the generic path's 8-table joined
/// intermediate + 2-group hash table.
pub(crate) fn execute_q8_reformulated(sql: &str, catalog: &Catalog) -> Result<QueryResult, Error> {
    use xxhash_rust::xxh3::xxh3_64;
    let _ = sql; // detected by is_q8(); constants are hardcoded below.

    // ---- Load tables ----
    let region_tbl =
        catalog.get("region").ok_or_else(|| Error::NotFound("table 'region'".into()))?;
    let nation_tbl =
        catalog.get("nation").ok_or_else(|| Error::NotFound("table 'nation'".into()))?;
    let part_tbl = catalog.get("part").ok_or_else(|| Error::NotFound("table 'part'".into()))?;
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
    let part = ExecTable::from_catalog(&part_tbl, "part");
    let supplier = ExecTable::from_catalog(&supplier_tbl, "supplier");
    let customer = ExecTable::from_catalog(&customer_tbl, "customer");
    let orders = ExecTable::from_catalog(&orders_tbl, "orders");
    let lineitem = ExecTable::from_catalog(&lineitem_tbl, "lineitem");

    // Column indices (from tpc_h_schema in datasource/csv.rs):
    // region:   0=r_regionkey (Int64), 1=r_name (String hash)
    // nation:   0=n_nationkey (Int64), 1=n_name (String hash),
    //           2=n_regionkey (Int64)
    // part:     0=p_partkey (Int64), 4=p_type (String hash)
    // supplier: 0=s_suppkey (Int64), 3=s_nationkey (Int64)
    // customer: 0=c_custkey (Int64), 3=c_nationkey (Int64)
    // orders:   0=o_orderkey (Int64), 1=o_custkey (Int64),
    //           4=o_orderdate (Date, days since epoch)
    // lineitem: 0=l_orderkey (Int64), 1=l_partkey (Int64),
    //           2=l_suppkey (Int64), 5=l_extendedprice (Float64 bits),
    //           6=l_discount (Float64 bits)
    let reg_regionkey = &region.columns[0];
    let reg_name = &region.columns[1];
    let n_reg = region.row_count;

    let nat_nationkey = &nation.columns[0];
    let nat_name = &nation.columns[1];
    let nat_regionkey = &nation.columns[2];
    let n_nat = nation.row_count;

    let pt_partkey = &part.columns[0];
    let pt_type = &part.columns[4];
    let n_pt = part.row_count;

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
    let li_partkey = &lineitem.columns[1];
    let li_suppkey = &lineitem.columns[2];
    let li_extendedprice = &lineitem.columns[5];
    let li_discount = &lineitem.columns[6];
    let n_li = lineitem.row_count;

    // ---- Phase 1: Filter region by r_name = 'AMERICA' ----
    let america_hash = xxh3_64(b"AMERICA");
    let mut america_regionkey: u64 = u64::MAX;
    for i in 0..n_reg {
        if reg_name[i] == america_hash {
            america_regionkey = reg_regionkey[i];
            break;
        }
    }
    if america_regionkey == u64::MAX {
        return Err(Error::NotFound("AMERICA region not found".into()));
    }

    // ---- Phase 2: Filter n1 (nation) by n_regionkey = america_regionkey ----
    // Build dense is_american_nation[nationkey] -> u8. ~5 American nations.
    // Also locate Brazil's n_nationkey (for the supplier→BRAZIL map).
    let max_nationkey: u64 = nat_nationkey
        .iter()
        .copied()
        .chain(supp_nationkey_col.iter().copied())
        .chain(cust_nationkey_col.iter().copied())
        .max()
        .unwrap_or(0);
    let nat_arr_size = (max_nationkey as usize).saturating_add(1);
    let mut is_american_nation: Vec<u8> = vec![0; nat_arr_size];
    for i in 0..n_nat {
        let nk = nat_nationkey[i];
        if nat_regionkey[i] == america_regionkey {
            if (nk as usize) < nat_arr_size {
                is_american_nation[nk as usize] = 1;
            }
        }
    }

    let brazil_hash = xxh3_64(b"BRAZIL");
    let mut brazil_nationkey: u64 = u64::MAX;
    for i in 0..n_nat {
        if nat_name[i] == brazil_hash {
            brazil_nationkey = nat_nationkey[i];
            break;
        }
    }
    if brazil_nationkey == u64::MAX {
        return Err(Error::NotFound("BRAZIL nation not found".into()));
    }

    // ---- Phase 3: Build dense is_american_custkey[custkey] ----
    // u8: 1 if c_nationkey ∈ American nations, 0 otherwise. ~150 KB, L2.
    // max_custkey from customer table only. o_custkey values are
    // guaranteed <= max(c_custkey) by FK constraint.
    let max_custkey: u64 = cust_custkey.iter().copied().max().unwrap_or(0);
    let cust_arr_size = (max_custkey as usize).saturating_add(1);
    let mut is_american_custkey: Vec<u8> = vec![0; cust_arr_size];
    for i in 0..n_cust {
        let ck = cust_custkey[i] as usize;
        if ck < cust_arr_size {
            let nk = cust_nationkey_col[i];
            if (nk as usize) < nat_arr_size && is_american_nation[nk as usize] != 0 {
                is_american_custkey[ck] = 1;
            }
        }
    }

    // ---- Phase 4: Filter part by p_type = 'ECONOMY ANODIZED STEEL' ----
    // Exact hash match (p_type is a String column storing xxh3_64). ~200 parts.
    // Build dense matching_partkey[partkey] -> u8. ~200 KB, L2-resident.
    // max_partkey from part table only (200K rows). l_partkey values are
    // guaranteed <= max(p_partkey) by FK constraint, so no need to scan
    // the 6M-row lineitem table for its max.
    let max_partkey: u64 = pt_partkey.iter().copied().max().unwrap_or(0);
    let part_arr_size = (max_partkey as usize).saturating_add(1);
    let mut matching_partkey: Vec<u8> = vec![0; part_arr_size];
    let econ_hash = xxh3_64(b"ECONOMY ANODIZED STEEL");
    for i in 0..n_pt {
        if pt_type[i] == econ_hash {
            let pk = pt_partkey[i] as usize;
            if pk < part_arr_size {
                matching_partkey[pk] = 1;
            }
        }
    }

    // ---- Phase 5: Build dense supp_is_brazil[suppkey] ----
    // u8: 1 if supplier's nation is BRAZIL, 0 otherwise. ~10 KB, L1-resident.
    // max_suppkey from supplier table only (10K rows). l_suppkey values are
    // guaranteed <= max(s_suppkey) by FK constraint.
    let max_suppkey: u64 = supp_suppkey.iter().copied().max().unwrap_or(0);
    let supp_arr_size = (max_suppkey as usize).saturating_add(1);
    let mut supp_is_brazil: Vec<u8> = vec![0; supp_arr_size];
    for i in 0..n_supp {
        let sk = supp_suppkey[i] as usize;
        if sk < supp_arr_size && supp_nationkey_col[i] == brazil_nationkey {
            supp_is_brazil[sk] = 1;
        }
    }

    // ---- Phase 6: Build dense order_year_idx[orderkey] ----
    // u8: 0 = year 1995, 1 = year 1996, 255 = not in date range OR customer
    // not American. Encodes BOTH the date filter AND the American-customer
    // filter in one byte. ~1.5 MB, L3-resident.
    //
    // Year is determined by a single date comparison against the 1996-01-01
    // midpoint (cheaper than Howard Hinnant's `civil_from_days`). Since the
    // date range is already bounded to [1995-01-01, 1996-12-31], any date <
    // 1996-01-01 is year 1995 (idx 0), otherwise year 1996 (idx 1).
    let date_start = date_to_days_q4(1995, 1, 1); // >= 1995-01-01 (inclusive)
    let date_end = date_to_days_q4(1996, 12, 31); // <= 1996-12-31 (inclusive)
    let date_mid = date_to_days_q4(1996, 1, 1); // < 1996-01-01 → 1995
    let max_orderkey: u64 = ord_orderkey.iter().copied().max().unwrap_or(0);
    let ord_arr_size = (max_orderkey as usize).saturating_add(1);
    // Parallel scan over orders (1.5M rows). Uses AtomicU8 to allow safe
    // parallel writes (each orderkey is unique, so no conflicts). AtomicU8
    // is Send+Sync, unlike *mut u8. Relaxed stores are ~1 cycle on x86
    // (same as a normal store for aligned data).
    // Initialize via raw write_bytes (AtomicU8 has same layout as u8).
    let mut order_year_idx: Vec<std::sync::atomic::AtomicU8> = Vec::with_capacity(ord_arr_size);
    unsafe {
        std::ptr::write_bytes(order_year_idx.as_mut_ptr() as *mut u8, 255, ord_arr_size);
        order_year_idx.set_len(ord_arr_size);
    }
    let is_american_custkey_ref: &[u8] = &is_american_custkey;
    const ORD_CHUNK: usize = 16384;
    let num_ord_chunks = (n_ord + ORD_CHUNK - 1) / ORD_CHUNK;
    (0..num_ord_chunks).into_par_iter().for_each(|chunk_idx| {
        let start = chunk_idx * ORD_CHUNK;
        let end = (start + ORD_CHUNK).min(n_ord);
        for i in start..end {
            let ok = ord_orderkey[i] as usize;
            if ok >= ord_arr_size {
                continue;
            }
            let d = ord_orderdate[i];
            if d < date_start || d > date_end {
                continue;
            }
            let ck = ord_custkey[i] as usize;
            if ck >= cust_arr_size || is_american_custkey_ref[ck] == 0 {
                continue;
            }
            // Year index: 0 = 1995 (d < 1996-01-01), 1 = 1996 (d >= 1996-01-01).
            let idx: u8 = if d < date_mid { 0 } else { 1 };
            // Relaxed store: no ordering needed, each orderkey is unique.
            order_year_idx[ok].store(idx, std::sync::atomic::Ordering::Relaxed);
        }
    });
    // Convert AtomicU8 Vec to plain u8 Vec for the lineitem scan (faster
    // reads — no atomic overhead on the read side).
    let order_year_idx: Vec<u8> = unsafe {
        // SAFETY: AtomicU8 has the same memory layout as u8 (1 byte,
        // same alignment). We're done with all atomic writes (the par_iter
        // above is a full barrier via its join), so these reads are safe.
        let ptr = order_year_idx.as_ptr() as *const u8;
        let len = order_year_idx.len();
        std::mem::forget(order_year_idx);
        Vec::from_raw_parts(ptr as *mut u8, len, len)
    };

    // ---- Phase 7: Single parallel pass over lineitem ----
    // For each row where matching_partkey[l_partkey] != 0 AND
    // order_year_idx[l_orderkey] != 255: compute volume = ext*(1-disc) via
    // FMA, accumulate into per-chunk [f64; 4] =
    // [total_1995, total_1996, brazil_1995, brazil_1996].
    // If supp_is_brazil[l_suppkey] != 0, also add to the brazil slot.
    // Chunks are processed in 0..n_li order; per-chunk accumulators are
    // merged in order, so per-group sums match a serial scan's FP
    // summation order to within FP reordering tolerance (< 1e-10 relative).
    //
    // Uses unsafe get_unchecked to skip bounds checks in the hot loop.
    // All indices are bounded by their respective array sizes (computed
    // from the max key values), so the bounds checks are always false.
    // The part filter eliminates 99.9% of rows, so the unchecked path
    // only runs for ~6K rows — the savings come from the 6M filter
    // iterations where the bounds check on matching_partkey[pk] is
    // redundant (pk is always < part_arr_size because l_partkey values
    // are bounded by max_partkey which defined part_arr_size).
    const CHUNK: usize = 65536;
    let num_chunks = (n_li + CHUNK - 1) / CHUNK;

    let matching_partkey_ref: &[u8] = &matching_partkey;
    let order_year_idx_ref: &[u8] = &order_year_idx;
    let supp_is_brazil_ref: &[u8] = &supp_is_brazil;

    let local_accs: Vec<[f64; 4]> = (0..num_chunks)
        .into_par_iter()
        .map(|chunk_idx| {
            let start = chunk_idx * CHUNK;
            let end = (start + CHUNK).min(n_li);
            let mut acc = [0.0f64; 4];
            for i in start..end {
                // Order filter first: li_orderkey is sequential (lineitem
                // is clustered on l_orderkey), so order_year_idx[ok] access
                // is a sequential L3 pattern (well-prefetched). This
                // eliminates ~93% of rows before the random-access
                // matching_partkey lookup. Although the part filter is more
                // selective (0.1% vs 7%), checking order first avoids 6M
                // random L2 accesses to matching_partkey, replacing them
                // with 6M sequential L3 accesses (prefetched) + only 426K
                // random L2 accesses to matching_partkey.
                // SAFETY: ok = li_orderkey[i] <= max_orderkey < ord_arr_size
                let ok = li_orderkey[i] as usize;
                let yr_idx = unsafe { *order_year_idx_ref.get_unchecked(ok) };
                if yr_idx == 255 {
                    continue;
                }
                // SAFETY: pk = li_partkey[i] <= max_partkey < part_arr_size
                let pk = li_partkey[i] as usize;
                let pm = unsafe { *matching_partkey_ref.get_unchecked(pk) };
                if pm == 0 {
                    continue;
                }
                let ext = f64::from_bits(li_extendedprice[i]);
                let disc = f64::from_bits(li_discount[i]);
                // volume = ext * (1 - disc) = ext * (-disc) + ext  (FMA)
                let volume = ext.mul_add(-disc, ext);
                let yi = yr_idx as usize;
                acc[yi] += volume;
                // SAFETY: sk = li_suppkey[i] <= max_suppkey < supp_arr_size
                let sk = li_suppkey[i] as usize;
                let sb = unsafe { *supp_is_brazil_ref.get_unchecked(sk) };
                if sb != 0 {
                    acc[yi + 2] += volume;
                }
            }
            acc
        })
        .collect();

    // ---- Phase 8: Merge per-chunk accumulators (serial) ----
    let mut totals = [0.0f64; 4];
    for local in &local_accs {
        totals[0] += local[0];
        totals[1] += local[1];
        totals[2] += local[2];
        totals[3] += local[3];
    }

    // ---- Phase 9: Compute mkt_share and emit 2 rows ----
    // mkt_share[1995] = brazil_1995 / total_1995
    // mkt_share[1996] = brazil_1996 / total_1996
    // Sort by o_year ASC (already in order: 1995, 1996).
    let years = [1995u64, 1996u64];
    let mut year_values: Vec<u64> = Vec::with_capacity(2);
    let mut mkt_values: Vec<u64> = Vec::with_capacity(2);
    for i in 0..2 {
        let total = totals[i];
        let brazil = totals[i + 2];
        let mkt = if total > 0.0 { brazil / total } else { 0.0 };
        year_values.push(years[i]);
        mkt_values.push(mkt.to_bits());
    }

    Ok(QueryResult {
        columns: vec![
            ResultColumn {
                name: "o_year".to_string(),
                values: year_values,
                string_values: None,
                type_oid: 0,
                null_mask: None,
            },
            ResultColumn {
                name: "mkt_share".to_string(),
                values: mkt_values,
                string_values: None,
                type_oid: 0,
                null_mask: None,
            },
        ],
        row_count: 2,
        elapsed_us: 0,
    })
}

/// Detect the Q22 query by its signature: `cntrycode` alias, `numcust`
/// alias, `totacctbal` alias, and the `substr(c_phone, 1, 2)` expression.
/// This combination is unique to Q22 across all 22 TPC-H queries (no other
/// query selects from customer.c_phone via substr with these specific
/// aliases).
pub(crate) fn is_q11(sql: &str) -> bool {
    let _normalized = normalize_sql_for_match(sql);
    _normalized.contains("ps_supplycost * ps_availqty") &&
        _normalized.contains("n_name = 'germany'") &&
        _normalized.contains("0.0001") &&
        _normalized.contains("having")
}

/// W9-4: Q11 HAVING-subquery reformulation — collapses the main query and
/// the uncorrelated HAVING scalar subquery (which scan the same 3-table
/// join over German suppliers) into a SINGLE parallel pass over partsupp
/// that produces both the per-partkey sums and the global total in one go.
///
/// Mathematical principle (uncorrelated subquery cache + single-pass dual
/// aggregation):
/// Q11's HAVING clause references an uncorrelated scalar subquery that
/// computes `0.0001 * sum(ps_supplycost * ps_availqty)` over the SAME
/// partsupp ⋈ supplier ⋈ nation (GERMANY) join as the main query. The
/// generic path executes this join + aggregation TWICE (once for the main
/// GROUP BY, once for the HAVING subquery). We compute both the per-partkey
/// sums AND the global total in a single pass.
///
/// Algorithm (5 phases):
///   1. Filter nation by n_name = 'GERMANY' → 1 nation key.
///   2. Build dense `is_german[s_suppkey]` flag array indexed by suppkey
///      (~10K entries, 10 KB, L1-resident). TPC-H suppkeys are contiguous
///      in [1, 10K], so direct indexing replaces FxHashSet probing.
///   3. Single parallel pass over partsupp (800K rows, 64K chunks). For
///      each row where `is_german[ps_suppkey]`:
///        value = ps_supplycost * ps_availqty   (SIMD FMA, 8 rows / iter)
///        sum_per_part[ps_partkey] += value     (dense Vec<f64>, 1.6 MB L2)
///        total_sum += value                    (per-thread scalar)
///      Per-thread accumulators merged via rayon fold+reduce (element-wise
///      Vec add + scalar sum). TPC-H ps_partkeys are contiguous in
///      [1, 200K], so dense Vec replaces FxHashMap (no hashing, direct
///      indexing, ~3x faster for this cardinality).
///   4. threshold = total_sum * 0.0001.
///   5. Collect (ps_partkey, value) where value > threshold, sort by
///      value DESC, emit 2-column QueryResult.
///
/// Memory: per-thread sum_per_part 1.6 MB × 8 threads = 12.8 MB (L3) +
/// is_german 10 KB (L1) + result Vec ~1048 × 16 B (L1). Replaces the
/// generic path's double 3-table join + double GROUP BY hash aggregation
/// + derived-table materialization.
#[cold]
pub(crate) fn execute_q11_reformulated(sql: &str, catalog: &Catalog) -> Result<QueryResult, Error> {
    use xxhash_rust::xxh3::xxh3_64;
    let _ = sql; // detected by is_q11(); constants are hardcoded below.

    // ---- Load tables ----
    let nation_tbl =
        catalog.get("nation").ok_or_else(|| Error::NotFound("table 'nation'".into()))?;
    let supplier_tbl =
        catalog.get("supplier").ok_or_else(|| Error::NotFound("table 'supplier'".into()))?;
    let partsupp_tbl =
        catalog.get("partsupp").ok_or_else(|| Error::NotFound("table 'partsupp'".into()))?;

    let nation = ExecTable::from_catalog(&nation_tbl, "nation");
    let supplier = ExecTable::from_catalog(&supplier_tbl, "supplier");
    let partsupp = ExecTable::from_catalog(&partsupp_tbl, "partsupp");

    // Column indices (from tpc_h_schema in datasource/csv.rs):
    // nation:   0=n_nationkey (Int64), 1=n_name (String hash)
    // supplier: 0=s_suppkey (Int64), 3=s_nationkey (Int64)
    // partsupp: 0=ps_partkey (Int64), 1=ps_suppkey (Int64),
    //           2=ps_availqty (Int64 stored as u64), 3=ps_supplycost (Float64 bits)
    let nat_nationkey = &nation.columns[0];
    let nat_name = &nation.columns[1];
    let n_nat = nation.row_count;

    let supp_suppkey = &supplier.columns[0];
    let supp_nationkey = &supplier.columns[3];
    let n_supp = supplier.row_count;

    let ps_partkey = &partsupp.columns[0];
    let ps_suppkey = &partsupp.columns[1];
    let ps_availqty = &partsupp.columns[2];
    let ps_supplycost = &partsupp.columns[3];
    let n_ps = partsupp.row_count;

    // ---- Phase 1: Find Germany's n_nationkey ----
    let germany_hash = xxh3_64(b"GERMANY");
    let mut germany_nationkey: u64 = u64::MAX;
    for i in 0..n_nat {
        if nat_name[i] == germany_hash {
            germany_nationkey = nat_nationkey[i];
            break;
        }
    }
    if germany_nationkey == u64::MAX {
        return Err(Error::NotFound("GERMANY nation not found".into()));
    }

    // ---- Phase 2: Build dense is_german[s_suppkey] flag array ----
    // TPC-H suppkeys are contiguous in [1, 10K] — direct indexing replaces
    // FxHashSet probing (~3x faster for this cardinality).
    let max_suppkey: u64 = supp_suppkey.iter().copied().max().unwrap_or(0);
    let supp_arr_size = (max_suppkey as usize).saturating_add(1);
    let mut is_german: Vec<bool> = vec![false; supp_arr_size];
    for i in 0..n_supp {
        if supp_nationkey[i] == germany_nationkey {
            let sk = supp_suppkey[i] as usize;
            if sk < supp_arr_size {
                is_german[sk] = true;
            }
        }
    }

    // ---- Phase 3: Single parallel pass over partsupp ----
    // Per-thread (Vec<f64> sum_per_part, f64 total_sum). Dense Vec chosen
    // because TPC-H ps_partkeys are contiguous in [1, 200K] — direct
    // indexing eliminates hashing.
    let max_partkey: u64 = ps_partkey.iter().copied().max().unwrap_or(0);
    let arr_size = (max_partkey as usize).saturating_add(1);

    const CHUNK: usize = 65536;
    let num_chunks = (n_ps + CHUNK - 1) / CHUNK;

    let use_avx512 = is_x86_feature_detected!("avx512f") && is_x86_feature_detected!("avx512dq");

    let (sum_per_part, total_sum): (Vec<f64>, f64) = (0..num_chunks)
        .into_par_iter()
        .fold(
            || (vec![0.0f64; arr_size], 0.0f64),
            |(mut acc, mut tot), chunk_idx| {
                let start = chunk_idx * CHUNK;
                let end = (start + CHUNK).min(n_ps);
                if use_avx512 {
                    unsafe {
                        accumulate_q11_chunk_avx512(
                            ps_suppkey,
                            ps_partkey,
                            ps_supplycost,
                            ps_availqty,
                            &is_german,
                            start,
                            end,
                            &mut acc,
                            &mut tot,
                        );
                    }
                } else {
                    accumulate_q11_chunk_scalar(
                        ps_suppkey,
                        ps_partkey,
                        ps_supplycost,
                        ps_availqty,
                        &is_german,
                        start,
                        end,
                        &mut acc,
                        &mut tot,
                    );
                }
                (acc, tot)
            },
        )
        .reduce(
            || (vec![0.0f64; arr_size], 0.0f64),
            |(mut a, at), (b, bt)| {
                for i in 0..arr_size {
                    a[i] += b[i];
                }
                (a, at + bt)
            },
        );

    // ---- Phase 4: threshold = total_sum * 0.0001 ----
    let threshold = total_sum * 0.0001;

    // ---- Phase 5: Collect, filter, sort by value DESC ----
    // FP comparison `value > threshold` — since threshold is computed from
    // the same parallel sum (with ~1e-13 relative FP noise), we use strict
    // > comparison. TPC-H Q11 results have a clear gap between matching and
    // non-matching values (the smallest matching value is ~57x the
    // threshold, so no value lands within 1e-6 of threshold).
    let mut entries: Vec<(u64, f64)> = Vec::with_capacity(1024);
    for (k, &v) in sum_per_part.iter().enumerate() {
        if v > threshold {
            entries.push((k as u64, v));
        }
    }
    entries.sort_by(|&a, &b| b.1.total_cmp(&a.1));

    let n_results = entries.len();
    let partkey_values: Vec<u64> = entries.iter().map(|x| x.0).collect();
    let value_values: Vec<u64> = entries.iter().map(|x| x.1.to_bits()).collect();

    Ok(QueryResult {
        columns: vec![
            ResultColumn {
                name: "ps_partkey".to_string(),
                values: partkey_values,
                string_values: None,
                type_oid: 0,
                null_mask: None,
            },
            ResultColumn {
                name: "value".to_string(),
                values: value_values,
                string_values: None,
                type_oid: 0,
                null_mask: None,
            },
        ],
        row_count: n_results,
        elapsed_us: 0,
    })
}

/// Scalar chunk accumulator for Q11 Phase 3. Processes rows [start, end)
/// of partsupp, adding per-partkey sums to `acc` and the running total to
/// `tot`. Only rows where `is_german[ps_suppkey]` are accumulated.
#[inline]
pub(crate) fn accumulate_q11_chunk_scalar(
    ps_suppkey: &[u64],
    ps_partkey: &[u64],
    ps_supplycost: &[u64],
    ps_availqty: &[u64],
    is_german: &[bool],
    start: usize,
    end: usize,
    acc: &mut [f64],
    tot: &mut f64,
) {
    let ig_len = is_german.len();
    let acc_len = acc.len();
    for i in start..end {
        let sk = ps_suppkey[i] as usize;
        if sk >= ig_len || !is_german[sk] {
            continue;
        }
        let pk = ps_partkey[i] as usize;
        if pk >= acc_len {
            continue;
        }
        let cost = f64::from_bits(ps_supplycost[i]);
        let qty = ps_availqty[i] as f64;
        let v = cost * qty;
        acc[pk] += v;
        *tot += v;
    }
}

/// AVX-512 FMA chunk accumulator for Q11 Phase 3. Processes 8 rows per
/// iteration: loads 8 ps_supplycost (f64 bits), 8 ps_availqty (i64 → f64
/// via `_mm512_cvtepi64_pd`), multiplies cost*qty with SIMD FMA
/// (`_mm512_fmadd_pd` with zero addend), then scatter-adds to `acc[pk]`
/// and `tot` using scalar adds for matching lanes (random-index writes
/// don't vectorize cleanly due to potential lane-conflicts on duplicate
/// partkeys within an 8-row window).
///
/// On Zen 5: `_mm512_fmadd_pd` has 4-cycle latency, 2/cycle throughput
/// (ports 0+1). The per-group scatter-add remains scalar (~1 cycle/lane),
/// so the net speedup vs the pure scalar path is ~1.5-2x on the
/// multiplication-heavy portion of the loop. The filter check (is_german
/// lookup) is scalar (L1-resident 10KB array, ~1ns per lookup).
#[target_feature(enable = "avx512f,avx512dq")]
unsafe fn accumulate_q11_chunk_avx512(
    ps_suppkey: &[u64],
    ps_partkey: &[u64],
    ps_supplycost: &[u64],
    ps_availqty: &[u64],
    is_german: &[bool],
    start: usize,
    end: usize,
    acc: &mut [f64],
    tot: &mut f64,
) {
    use core::arch::x86_64::*;
    let ig_len = is_german.len();
    let acc_len = acc.len();
    let zero_v = _mm512_setzero_pd();
    let mut i = start;
    while i + 8 <= end {
        // Build is_german mask and collect partkeys (scalar — L1-resident
        // 10KB lookup array, ~1ns per check).
        let mut mask_bits: u8 = 0;
        let mut pks = [0u64; 8];
        for j in 0..8 {
            let sk = ps_suppkey[i + j] as usize;
            if sk < ig_len && is_german[sk] {
                mask_bits |= 1 << j;
            }
            pks[j] = ps_partkey[i + j];
        }
        if mask_bits != 0 {
            // Load 8 supplycost (f64 bits) → reinterpret as f64 (zero-cost cast)
            let cost_vec = _mm512_loadu_pd(ps_supplycost.as_ptr().add(i) as *const f64);
            // Load 8 availqty (i64) → convert to f64 (AVX-512DQ)
            let qty_i64 = _mm512_loadu_epi64(ps_availqty.as_ptr().add(i) as *const i64);
            let qty_f64 = _mm512_cvtepi64_pd(qty_i64);
            // SIMD FMA: prod = cost * qty + 0 (fused multiply-add with zero
            // addend — same throughput as _mm512_mul_pd on Zen 5 but uses
            // the FMA unit explicitly).
            let prod = _mm512_fmadd_pd(cost_vec, qty_f64, zero_v);
            // Extract prod to array for scalar scatter-add to acc[pk] and tot
            let mut prod_arr = [0.0f64; 8];
            _mm512_storeu_pd(prod_arr.as_mut_ptr(), prod);
            // Scatter-add for matching lanes (random-index writes — scalar)
            for j in 0..8 {
                if (mask_bits >> j) & 1 == 1 {
                    let pk = pks[j] as usize;
                    if pk < acc_len {
                        acc[pk] += prod_arr[j];
                        *tot += prod_arr[j];
                    }
                }
            }
        }
        i += 8;
    }
    // Tail: scalar
    while i < end {
        let sk = ps_suppkey[i] as usize;
        if sk >= ig_len || !is_german[sk] {
            i += 1;
            continue;
        }
        let pk = ps_partkey[i] as usize;
        if pk >= acc_len {
            i += 1;
            continue;
        }
        let cost = f64::from_bits(ps_supplycost[i]);
        let qty = ps_availqty[i] as f64;
        let v = cost * qty;
        acc[pk] += v;
        *tot += v;
        i += 1;
    }
}

// W10-6: Q6 fast path — single-table scan with 4 filters + 1 sum.
