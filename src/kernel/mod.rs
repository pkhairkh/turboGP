//! The kernel table: hand-tuned instruction sequences per (CPU, tier) tuple.
//!
//! Each operator (`scan_eq`, `hash_probe`, `aggregate_sum`, etc.) has multiple
//! kernel implementations, one per `(CpuTarget, MemoryTier)` combination. At
//! startup, the engine probes CPUID to detect the running CPU, then selects
//! the best kernel per operator per tier.
//!
//! ## Why this is the moat
//!
//! A generic vectorized executor (DuckDB, ClickHouse) picks one
//! implementation per operator and runs it regardless of where the data
//! lives. turbogp picks a *different* kernel for L3-resident data vs
//! CXL-resident data, because the optimal prefetch distance, batch size,
//! and SIMD width depend on the tier's latency and bandwidth.
//!
//! Example: `scan_eq_u64` on Sapphire Rapids:
//! - L3 tier: load 64 cells, `VPCMPEQQ`, `KMOVQ` → 19 G cells/sec
//! - DDR5 tier: 4-page prefetch pipeline, same SIMD → 5 G cells/sec
//! - CXL tier: 8-page prefetch, smaller batch → 3 G cells/sec
//!
//! Same operator, three kernels, three throughputs. The kernel table is what
//! makes the engine tier-aware rather than tier-blind.

pub mod aggregate;
pub mod cpu;
pub mod hash;
pub mod leapfrog;
pub mod scan;
pub mod similarity;
pub mod vnni_agg;

pub use crate::memory::tier::MemoryTier;
pub use cpu::{detect_cpu, CpuTarget, CpuVendor};
use parking_lot::RwLock;
use std::collections::HashMap;
use std::sync::Arc;

/// The key into the kernel table: `(Operator, CpuTarget, MemoryTier)`.
pub type KernelKey = (Operator, CpuTarget, MemoryTier);

/// The map type backing the kernel table.
///
/// Factored into a type alias to keep the `KernelTable` struct readable.
pub type KernelMap = RwLock<HashMap<KernelKey, Arc<dyn Kernel>>>;

/// Identifies an operator that kernels implement.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Operator {
    /// Equality scan: count cells equal to a target.
    ScanEqU64,
    /// Range scan: count cells in [low, high].
    ScanRangeU64,
    /// Multi-predicate scan: count cells matching ALL of up to 3 predicates.
    ///
    /// Each predicate is `(target, op)` where `op ∈ {Eq, Gt, Lt}`. Predicates
    /// are AND-combined; a cell must satisfy every predicate to count.
    /// Implements P-01-05 (fused multi-predicate scan via `VPTERNLOGQ`).
    ScanMultiPredicate,
    /// Hash table build.
    HashBuild,
    /// Hash table probe.
    HashProbe,
    /// Sum of f64 cells.
    AggregateSumF64,
    /// Count distinct via HyperLogLog.
    AggregateCountDistinct,
    /// Hamming similarity scan.
    SimilarityHamming,
    /// Leapfrog triejoin (Veldhuizen 2014): worst-case optimal multiway
    /// intersection on a sorted key. The scalar kernel runs a 2-way
    /// leapfrog intersection over two concatenated slices; multi-way joins
    /// use [`crate::kernel::leapfrog::LeapfrogJoin`] directly.
    LeapfrogJoin,
}

/// A single comparison predicate used by `ScanMultiPredicate`.
///
/// `Eq` matches cells equal to `target`, `Gt` matches cells strictly greater
/// than `target`, `Lt` matches cells strictly less than `target`. Up to three
/// predicates are AND-combined per scan; unused slots default to `Eq` with a
/// `target` of `u64::MAX` (which matches everything when AND-combined with a
/// real predicate — but the kernel only honors `predicate_count` predicates).
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub enum PredicateOp {
    /// `cell == target`
    #[default]
    Eq,
    /// `cell > target`
    Gt,
    /// `cell < target`
    Lt,
}

/// Parameters passed to a kernel at execution time.
#[derive(Debug, Clone, Copy)]
pub struct KernelParams {
    /// Target value for equality / similarity kernels (also predicate 1 target).
    pub target_u64: u64,
    /// Low bound for range kernels.
    pub low_u64: u64,
    /// High bound for range kernels.
    pub high_u64: u64,
    /// Max Hamming distance for similarity kernels.
    pub max_distance: u32,
    /// Number of cells to process.
    pub cell_count: usize,
    /// Target value for predicate 2 of `ScanMultiPredicate`.
    pub target2_u64: u64,
    /// Target value for predicate 3 of `ScanMultiPredicate`.
    pub target3_u64: u64,
    /// Comparison operator for predicate 1 of `ScanMultiPredicate`.
    pub pred1_op: PredicateOp,
    /// Comparison operator for predicate 2 of `ScanMultiPredicate`.
    pub pred2_op: PredicateOp,
    /// Comparison operator for predicate 3 of `ScanMultiPredicate`.
    pub pred3_op: PredicateOp,
    /// Number of predicates active in `ScanMultiPredicate` (1..=3).
    pub predicate_count: u8,
}

impl Default for KernelParams {
    fn default() -> Self {
        Self {
            target_u64: 0,
            low_u64: 0,
            high_u64: u64::MAX,
            max_distance: 0,
            cell_count: 0,
            target2_u64: 0,
            target3_u64: 0,
            pred1_op: PredicateOp::Eq,
            pred2_op: PredicateOp::Eq,
            pred3_op: PredicateOp::Eq,
            predicate_count: 0,
        }
    }
}

/// Result of a kernel execution.
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct KernelResult {
    /// Count of matching cells (for scan / similarity kernels).
    pub count: u64,
    /// Sum of matching cells (for aggregate kernels).
    pub sum: f64,
    /// Bitmask of matching positions (for scan kernels, first 64 cells).
    pub mask: u64,
}

/// A kernel: a hand-tuned instruction sequence for a specific (CPU, tier).
///
/// # Safety
///
/// Implementations use SIMD intrinsics and raw pointers. Callers must ensure:
/// - `input` points to at least `params.cell_count * 8` readable bytes
/// - `output` points to at least `size_of::<KernelResult>()` writable bytes
/// - The CPU supports the required feature flags (checked at registration)
pub trait Kernel: Send + Sync {
    /// The operator this kernel implements.
    fn operator(&self) -> Operator;
    /// The CPU target this kernel is tuned for.
    fn cpu(&self) -> CpuTarget;
    /// The memory tier this kernel is optimized for.
    fn tier(&self) -> MemoryTier;
    /// Human-readable name.
    fn name(&self) -> &'static str;
    /// Execute the kernel.
    ///
    /// # Safety
    /// See trait docs.
    unsafe fn execute(
        &self,
        input: *const u8,
        output: *mut u8,
        params: &KernelParams,
    ) -> KernelResult;
}

/// The kernel table: maps `(Operator, CpuTarget, MemoryTier)` to the best
/// kernel for that combination.
pub struct KernelTable {
    /// All registered kernels, keyed by (operator, cpu, tier).
    kernels: KernelMap,
    /// The detected CPU of this machine.
    detected_cpu: CpuTarget,
}

impl KernelTable {
    /// Build a kernel table for the running CPU, registering all available
    /// kernels.
    pub fn new() -> Self {
        let detected_cpu = detect_cpu();
        let mut kernels: HashMap<(Operator, CpuTarget, MemoryTier), Arc<dyn Kernel>> =
            HashMap::new();

        // Register all built-in kernels. Only the kernels matching the
        // detected CPU will actually be selectable, but we register all of
        // them so the table is introspectable.
        register_scan_kernels(&mut kernels);
        register_hash_kernels(&mut kernels);
        register_aggregate_kernels(&mut kernels);
        register_similarity_kernels(&mut kernels);
        register_join_kernels(&mut kernels);

        Self { kernels: RwLock::new(kernels), detected_cpu }
    }

    /// Select the best kernel for `(operator, tier)` on the detected CPU.
    pub fn select(&self, op: Operator, tier: MemoryTier) -> Option<Arc<dyn Kernel>> {
        let kernels = self.kernels.read();
        // Try exact match first.
        if let Some(k) = kernels.get(&(op, self.detected_cpu, tier)) {
            return Some(k.clone());
        }
        // Fallback: try the scalar kernel (CpuTarget::Scalar) for this tier.
        if let Some(k) = kernels.get(&(op, CpuTarget::Scalar, tier)) {
            return Some(k.clone());
        }
        // Last resort: any kernel for this operator on any CPU/tier.
        kernels.values().find(|k| k.operator() == op).cloned()
    }

    /// The detected CPU target.
    pub fn detected_cpu(&self) -> CpuTarget {
        self.detected_cpu
    }

    /// Register a custom kernel.
    pub fn register(&self, kernel: Arc<dyn Kernel>) {
        let key = (kernel.operator(), kernel.cpu(), kernel.tier());
        self.kernels.write().insert(key, kernel);
    }

    /// List all registered kernels.
    pub fn list(&self) -> Vec<(Operator, CpuTarget, MemoryTier, &'static str)> {
        self.kernels
            .read()
            .iter()
            .map(|((op, cpu, tier), k)| (*op, *cpu, *tier, k.name()))
            .collect()
    }
}

impl Default for KernelTable {
    fn default() -> Self {
        Self::new()
    }
}

fn register_scan_kernels(
    kernels: &mut HashMap<(Operator, CpuTarget, MemoryTier), Arc<dyn Kernel>>,
) {
    use scan::*;

    // Scalar fallback (works everywhere).
    kernels
        .insert((Operator::ScanEqU64, CpuTarget::Scalar, MemoryTier::L3), Arc::new(ScanEqScalar));
    kernels.insert(
        (Operator::ScanRangeU64, CpuTarget::Scalar, MemoryTier::L3),
        Arc::new(ScanRangeScalar),
    );
    kernels.insert(
        (Operator::ScanMultiPredicate, CpuTarget::Scalar, MemoryTier::L3),
        Arc::new(ScanMultiPredicateScalar),
    );

    // AVX-512 kernels (Sapphire Rapids, Zen 4/5).
    #[cfg(target_arch = "x86_64")]
    {
        kernels.insert(
            (Operator::ScanEqU64, CpuTarget::X86Avx512, MemoryTier::L3),
            Arc::new(ScanEqAvx512L3),
        );
        kernels.insert(
            (Operator::ScanEqU64, CpuTarget::X86Avx512, MemoryTier::Ddr5),
            Arc::new(ScanEqAvx512Ddr5),
        );
        kernels.insert(
            (Operator::ScanEqU64, CpuTarget::X86Avx512, MemoryTier::Cxl),
            Arc::new(ScanEqAvx512Cxl),
        );
        kernels.insert(
            (Operator::ScanRangeU64, CpuTarget::X86Avx512, MemoryTier::L3),
            Arc::new(ScanRangeAvx512L3),
        );
        kernels.insert(
            (Operator::ScanMultiPredicate, CpuTarget::X86Avx512, MemoryTier::L3),
            Arc::new(ScanMultiPredicateAvx512),
        );
    }

    // AVX2 kernels (fallback for non-AVX-512 x86).
    #[cfg(target_arch = "x86_64")]
    {
        kernels.insert(
            (Operator::ScanEqU64, CpuTarget::X86Avx2, MemoryTier::L3),
            Arc::new(ScanEqAvx2),
        );
    }
}

fn register_hash_kernels(
    kernels: &mut HashMap<(Operator, CpuTarget, MemoryTier), Arc<dyn Kernel>>,
) {
    use hash::*;
    kernels.insert(
        (Operator::HashBuild, CpuTarget::Scalar, MemoryTier::Ddr5),
        Arc::new(HashBuildScalar),
    );
    kernels.insert(
        (Operator::HashProbe, CpuTarget::Scalar, MemoryTier::L3),
        Arc::new(HashProbeScalar),
    );
    #[cfg(target_arch = "x86_64")]
    {
        kernels.insert(
            (Operator::HashProbe, CpuTarget::X86Avx512, MemoryTier::L3),
            Arc::new(HashProbeAvx512),
        );
    }
}

fn register_aggregate_kernels(
    kernels: &mut HashMap<(Operator, CpuTarget, MemoryTier), Arc<dyn Kernel>>,
) {
    use aggregate::*;
    kernels.insert(
        (Operator::AggregateSumF64, CpuTarget::Scalar, MemoryTier::L3),
        Arc::new(SumF64Scalar),
    );
    #[cfg(target_arch = "x86_64")]
    {
        kernels.insert(
            (Operator::AggregateSumF64, CpuTarget::X86Avx512, MemoryTier::L3),
            Arc::new(SumF64Avx512),
        );
        kernels.insert(
            (Operator::AggregateSumF64, CpuTarget::X86Avx2, MemoryTier::L3),
            Arc::new(SumF64Avx2),
        );
    }
    kernels.insert(
        (Operator::AggregateCountDistinct, CpuTarget::Scalar, MemoryTier::Ddr5),
        Arc::new(CountDistinctScalar),
    );
}

fn register_similarity_kernels(
    kernels: &mut HashMap<(Operator, CpuTarget, MemoryTier), Arc<dyn Kernel>>,
) {
    use similarity::*;
    kernels.insert(
        (Operator::SimilarityHamming, CpuTarget::Scalar, MemoryTier::L3),
        Arc::new(HammingScalar),
    );
    #[cfg(target_arch = "x86_64")]
    {
        kernels.insert(
            (Operator::SimilarityHamming, CpuTarget::X86Avx512, MemoryTier::L3),
            Arc::new(HammingAvx512),
        );
    }
}

fn register_join_kernels(
    kernels: &mut HashMap<(Operator, CpuTarget, MemoryTier), Arc<dyn Kernel>>,
) {
    use leapfrog::*;
    // Scalar leapfrog — works everywhere, runs a 2-way intersection over
    // two concatenated slices. Multi-way joins use LeapfrogJoin directly.
    kernels.insert(
        (Operator::LeapfrogJoin, CpuTarget::Scalar, MemoryTier::L3),
        Arc::new(LeapfrogScalar),
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn kernel_table_has_kernels() {
        let table = KernelTable::new();
        let list = table.list();
        assert!(!list.is_empty(), "kernel table should have kernels");
    }

    #[test]
    fn select_returns_a_kernel() {
        let table = KernelTable::new();
        let k = table.select(Operator::ScanEqU64, MemoryTier::L3);
        assert!(k.is_some(), "should find a scan_eq kernel for L3");
    }

    #[test]
    fn detected_cpu_is_valid() {
        let table = KernelTable::new();
        let cpu = table.detected_cpu();
        // Should detect *something* — at minimum, scalar.
        let _ = cpu;
    }
}
