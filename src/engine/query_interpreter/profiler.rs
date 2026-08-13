//! Per-query phase profiler. Behind the `profile` cargo feature.
//!
//! When enabled, accumulates elapsed nanoseconds per phase across all calls
//! during a single query. Thread-safe via `AtomicU64` (rayon parallel sections
//! can accumulate from every worker thread). At the end of each query
//! (`execute_interpreter`), the accumulator is printed to stderr in a
//! "phase: ms" table sorted descending by time, then reset for the next
//! query.
//!
//! ## Usage
//!
//! At the top of a hot phase, drop a guard:
//!
//! ```ignore
//! use crate::engine::query_interpreter::profiler::{PROFILER, Phase};
//! let _g = PROFILER.section(Phase::JoinHashProbe);
//! // ... hot work ...
//! // _g drops here, accumulating elapsed ns into join_hash_probe_ns.
//! ```
//!
//! For phases that must end before the surrounding scope ends (e.g. the join
//! build phase that lives in the same function as the probe phase), explicitly
//! drop the guard:
//!
//! ```ignore
//! let _g = PROFILER.section(Phase::JoinBuild);
//! // ... build work ...
//! drop(_g);  // stop JoinBuild timer before probe starts
//! ```
//!
//! ## Zero cost when disabled
//!
//! When the `profile` feature is OFF, `PROFILER` is a unit struct, `Phase`
//! is a degenerate enum, and `section()` returns `()`. The instrumentation
//! calls compile to no-ops.
//!
//! ## Limitations
//!
//! - The accumulator is a `static` — it accumulates across all calls during
//!   a single query (reset at the start of each `execute_interpreter` call).
//! - `Phase::ExprEval` wraps the `BinOp`/`Extract` arms of `expr.rs::eval`.
//!   Because `eval` recurses, nested guards will accumulate the same wall
//!   time multiple times (one per active guard). The reported `ExprEval`
//!   time is therefore an upper bound; subtract `FilterMask` and `Sort`
//!   (which both call `eval`) for a tighter estimate of pure projection
//!   expression cost.
//! - `Phase::Exists` wraps the per-row probe blocks in `expr.rs::eval`
//!   (`Expr2::Exists` arm) and the one-time build functions in
//!   `subquery.rs::build_exists_hashset` / `build_exists_multi_map`. Build
//!   time is counted once (in `subquery.rs`); per-row probe time is counted
//!   once (in `expr.rs`). No double-counting.

use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Instant;

// ---------------------------------------------------------------------------
// Feature ON: full implementation.
// ---------------------------------------------------------------------------

#[cfg(feature = "profile")]
pub static PROFILER: ProfileAccumulator = ProfileAccumulator::new();

#[cfg(feature = "profile")]
pub struct ProfileAccumulator {
    bloom_probe_ns: AtomicU64,
    join_hash_probe_ns: AtomicU64,
    join_build_ns: AtomicU64,
    exists_ns: AtomicU64,
    filter_mask_ns: AtomicU64,
    aggregate_ns: AtomicU64,
    sort_ns: AtomicU64,
    expr_eval_ns: AtomicU64,
}

#[cfg(feature = "profile")]
impl ProfileAccumulator {
    pub const fn new() -> Self {
        Self {
            bloom_probe_ns: AtomicU64::new(0),
            join_hash_probe_ns: AtomicU64::new(0),
            join_build_ns: AtomicU64::new(0),
            exists_ns: AtomicU64::new(0),
            filter_mask_ns: AtomicU64::new(0),
            aggregate_ns: AtomicU64::new(0),
            sort_ns: AtomicU64::new(0),
            expr_eval_ns: AtomicU64::new(0),
        }
    }

    /// Begin a timed section. The returned guard records `Instant::now()` on
    /// construction and accumulates elapsed nanoseconds into the phase counter
    /// on Drop. Call as:
    ///
    /// ```ignore
    /// let _g = PROFILER.section(Phase::BloomProbe);
    /// ```
    ///
    /// To end the section before its enclosing scope ends, `drop(_g);`
    /// explicitly.
    pub fn section(&self, phase: Phase) -> SectionGuard {
        SectionGuard { start: Instant::now(), phase }
    }

    /// Zero all phase counters. Called at the start of every
    /// `execute_interpreter` invocation so each query's profile is
    /// independent.
    pub fn reset(&self) {
        self.bloom_probe_ns.store(0, Ordering::Relaxed);
        self.join_hash_probe_ns.store(0, Ordering::Relaxed);
        self.join_build_ns.store(0, Ordering::Relaxed);
        self.exists_ns.store(0, Ordering::Relaxed);
        self.filter_mask_ns.store(0, Ordering::Relaxed);
        self.aggregate_ns.store(0, Ordering::Relaxed);
        self.sort_ns.store(0, Ordering::Relaxed);
        self.expr_eval_ns.store(0, Ordering::Relaxed);
    }

    /// Print the phase breakdown to stderr, sorted descending by time.
    /// Lines look like: `[profile] BloomProbe     :   123.456 ms`
    pub fn print(&self) {
        let mut rows: [(&str, u64); 8] = [
            ("BloomProbe     ", self.bloom_probe_ns.load(Ordering::Relaxed)),
            ("JoinHashProbe  ", self.join_hash_probe_ns.load(Ordering::Relaxed)),
            ("JoinBuild      ", self.join_build_ns.load(Ordering::Relaxed)),
            ("Exists         ", self.exists_ns.load(Ordering::Relaxed)),
            ("FilterMask     ", self.filter_mask_ns.load(Ordering::Relaxed)),
            ("Aggregate      ", self.aggregate_ns.load(Ordering::Relaxed)),
            ("Sort           ", self.sort_ns.load(Ordering::Relaxed)),
            ("ExprEval       ", self.expr_eval_ns.load(Ordering::Relaxed)),
        ];
        // Sort descending by accumulated ns.
        rows.sort_by(|a, b| b.1.cmp(&a.1));

        let total: u64 = rows.iter().map(|(_, ns)| ns).sum();
        eprintln!("[profile] ===== phase breakdown (total {:.3} ms) =====", total as f64 / 1e6);
        for (name, ns) in rows {
            if ns == 0 {
                continue;
            }
            let ms = ns as f64 / 1e6;
            let pct = if total > 0 { 100.0 * ns as f64 / total as f64 } else { 0.0 };
            eprintln!("[profile] {} : {:>10.3} ms  ({:>5.1}%)", name, ms, pct);
        }
        eprintln!("[profile] ============================================");
    }
}

#[cfg(feature = "profile")]
#[derive(Clone, Copy)]
pub enum Phase {
    BloomProbe,
    JoinHashProbe,
    JoinBuild,
    Exists,
    FilterMask,
    Aggregate,
    Sort,
    ExprEval,
}

#[cfg(feature = "profile")]
pub struct SectionGuard {
    start: Instant,
    phase: Phase,
}

#[cfg(feature = "profile")]
impl Drop for SectionGuard {
    fn drop(&mut self) {
        let elapsed = self.start.elapsed().as_nanos() as u64;
        let target: &AtomicU64 = match self.phase {
            Phase::BloomProbe => &PROFILER.bloom_probe_ns,
            Phase::JoinHashProbe => &PROFILER.join_hash_probe_ns,
            Phase::JoinBuild => &PROFILER.join_build_ns,
            Phase::Exists => &PROFILER.exists_ns,
            Phase::FilterMask => &PROFILER.filter_mask_ns,
            Phase::Aggregate => &PROFILER.aggregate_ns,
            Phase::Sort => &PROFILER.sort_ns,
            Phase::ExprEval => &PROFILER.expr_eval_ns,
        };
        // Relaxed is sufficient — we only need eventual visibility of the
        // sum, and rayon worker threads do not need to synchronize ordering
        // with each other for the timing to be useful.
        target.fetch_add(elapsed, Ordering::Relaxed);
    }
}

// ---------------------------------------------------------------------------
// Feature OFF: no-op stubs.
// ---------------------------------------------------------------------------

#[cfg(not(feature = "profile"))]
pub static PROFILER: ProfileAccumulator = ProfileAccumulator;

#[cfg(not(feature = "profile"))]
pub struct ProfileAccumulator;

// No-op RAII guard. Non-Copy so that `drop(_g)` actually drops something
// (avoids the `dropping_copy_types` warning that would otherwise fire
// for `drop(())` at the explicit-drop instrumentation sites in join.rs).
#[cfg(not(feature = "profile"))]
pub struct SectionGuard;

#[cfg(not(feature = "profile"))]
impl Drop for SectionGuard {
    #[inline(always)]
    fn drop(&mut self) {}
}

#[cfg(not(feature = "profile"))]
impl ProfileAccumulator {
    #[inline(always)]
    pub fn section(&self, _phase: Phase) -> SectionGuard { SectionGuard }
    #[inline(always)]
    pub fn reset(&self) {}
    #[inline(always)]
    pub fn print(&self) {}
}

#[cfg(not(feature = "profile"))]
#[derive(Clone, Copy)]
pub enum Phase {
    BloomProbe,
    JoinHashProbe,
    JoinBuild,
    Exists,
    FilterMask,
    Aggregate,
    Sort,
    ExprEval,
}
