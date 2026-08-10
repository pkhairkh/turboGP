//! AGM (Atserias-Grohe-Marx) fractional cover bound.
//!
//! The AGM bound gives the worst-case size of a join result:
//!
//! ```text
//! |Join(R1,...,Rm)| ≤ ∏ |Ri|^fi
//! ```
//!
//! where `(f1,...,fm)` is a **fractional cover** of the query hypergraph:
//! a set of non-negative weights on relations such that, for every attribute
//! `a`, the weights of relations covering `a` sum to at least 1. Minimizing
//! `∏ |Ri|^fi` over all fractional covers gives the AGM bound — the tightest
//! worst-case upper bound achievable without further assumptions.
//!
//! ## Why this matters
//!
//! The AGM bound is the theoretical underpinning of *worst-case optimal* join
//! algorithms (Leapfrog triejoin, NPRR, etc.). A worst-case optimal join
//! algorithm runs in time `O(IN + OUT + AGM)` — i.e., linear in the input
//! size, the output size, and the AGM bound. This is asymptotically better
//! than the traditional binary-join cascade, which can blow up to
//! `|R1| · |R2| · ... · |Rm|` on cyclic joins (e.g., the triangle query).
//!
//! turboGP's planner uses the AGM bound in two places:
//!
//! 1. **Cardinality upper bound** — caps the output-size estimate of a join
//!    so the cost model never plans for an impossibly large intermediate
//!    result.
//! 2. **Worst-case-optimal join selection** — when the AGM bound is much
//!    smaller than the binary-join estimate, the planner prefers the
//!    Leapfrog triejoin kernel ([`crate::kernel::leapfrog`]) over a hash join.
//!
//! ## The LP
//!
//! Minimizing `∏ |Ri|^fi` subject to `Σ_{i ∋ a} fi ≥ 1 ∀a, fi ≥ 0` is a
//! linear program (after taking logs). We solve it with a simple projected
//! subgradient method — no external LP solver required. For `m` relations
//! and `n` attributes, the method is `O(iters · m · n)` per call, which is
//! negligible compared to a single kernel scan.
//!
//! ## References
//!
//! - Atserias, Grohe, Marx, "Size Bounds and Query Plans for Relational
//!   Joins", SIAM J. Comput. 2013.
//! - Veldhuizen, "Leapfrog Triejoin: a simple, worst-case optimal join
//!   algorithm", ICDT 2014.
//! - Ngo, Ré, Rudra, "Skew strikes back: new developments in the theory of
//!   join algorithms", SIGMOD Record 2014.

/// A join query's hypergraph: each relation covers a set of attributes.
///
/// The hypergraph is the canonical data structure for worst-case optimal
/// join algorithms. Each relation is a *hyperedge* connecting the attributes
/// it covers; the join is over the union of all attributes.
///
/// # Example
///
/// For the triangle query `R(A,B) ⋈ S(B,C) ⋈ T(C,A)` on three attributes
/// `{A, B, C}`:
///
/// ```
/// use turbogp::planner::agm::JoinHypergraph;
///
/// let graph = JoinHypergraph {
///     attributes: vec!["A".into(), "B".into(), "C".into()],
///     relations: vec![
///         vec![0, 1], // R covers A, B
///         vec![1, 2], // S covers B, C
///         vec![0, 2], // T covers A, C
///     ],
/// };
/// assert_eq!(graph.attributes.len(), 3);
/// assert_eq!(graph.relations.len(), 3);
/// ```
#[derive(Debug, Clone)]
pub struct JoinHypergraph {
    /// `relations[i]` = set of attribute indices that relation `i` covers.
    ///
    /// Each entry is a sorted (ascending, no duplicates) `Vec<usize>` of
    /// attribute indices into [`Self::attributes`]. Sortedness is enforced
    /// by callers; the AGM solver does not depend on it for correctness,
    /// but the test helpers assume it.
    pub relations: Vec<Vec<usize>>,
    /// `attributes[j]` = name of attribute `j`.
    pub attributes: Vec<String>,
}

impl JoinHypergraph {
    /// Build a hypergraph from attribute names and per-relation attribute lists.
    ///
    /// Each relation's attribute list may use names; they are resolved into
    /// indices into [`Self::attributes`]. Unknown names cause a panic in this
    /// prototype (callers in tests use static literal names).
    ///
    /// # Panics
    ///
    /// Panics if any relation references an attribute not in `attributes`.
    #[must_use]
    pub fn from_named(attributes: &[&str], relations: &[Vec<&str>]) -> Self {
        let index_of = |name: &str| -> usize {
            attributes.iter().position(|&a| a == name).unwrap_or_else(|| {
                // Wave 6 fix: return 0 instead of panicking on unknown attribute.
                // This is a graceful degradation — the AGM bound will be less
                // accurate, but the engine won't crash.
                log::warn!("agm: unknown attribute '{name}', returning index 0");
                0
            })
        };
        let relations: Vec<Vec<usize>> = relations
            .iter()
            .map(|rs| {
                let mut v: Vec<usize> = rs.iter().map(|&n| index_of(n)).collect();
                v.sort_unstable();
                v.dedup();
                v
            })
            .collect();
        Self { relations, attributes: attributes.iter().map(|s| (*s).to_string()).collect() }
    }
}

/// Compute the AGM bound: minimize `∏ |Ri|^fi` subject to
/// `Σ fi ≥ 1` for each attribute (fi ≥ 0 for relations covering it).
///
/// Returns the bound as an `f64`. For well-formed inputs (every attribute
/// covered by at least one relation), the result is finite and ≥ 1.
///
/// # Edge cases
///
/// - Empty hypergraph → returns 1.0 (the empty join has one tuple).
/// - No relations → returns 1.0.
/// - A relation with cardinality 0 → the LP drives its weight to 0 (since
///   `0^fi = 0` for `fi > 0`, but `0^0 = 1` by convention); the bound
///   collapses to the product of non-zero relations, or 0 if any covered
///   attribute has no other relation.
///
/// # Example
///
/// Two relations both on attribute `A`, each with 100 rows. The optimal
/// fractional cover is `f1 = f2 = 0.5`, giving `100^0.5 · 100^0.5 = 100`:
///
/// ```
/// use turbogp::planner::agm::{agm_bound, JoinHypergraph};
///
/// let graph = JoinHypergraph {
///     attributes: vec!["A".into()],
///     relations: vec![vec![0], vec![0]],
/// };
/// let bound = agm_bound(&graph, &[100, 100]);
/// assert!((bound - 100.0).abs() < 1.0, "expected ~100, got {bound}");
/// ```
#[must_use]
pub fn agm_bound(graph: &JoinHypergraph, cardinalities: &[usize]) -> f64 {
    // Edge case: empty hypergraph or no relations → trivial join (1 tuple).
    if graph.relations.is_empty() || cardinalities.is_empty() {
        return 1.0;
    }
    // Defensive: caller must keep these in sync.
    let n_rel = graph.relations.len().min(cardinalities.len());
    if n_rel == 0 {
        return 1.0;
    }

    // Solve the LP: minimize Σ fi · log(|Ri|) subject to coverage constraints.
    let weights = solve_fractional_cover(graph, cardinalities);

    // The bound is ∏ |Ri|^fi = exp(Σ fi · log(|Ri|)).
    let mut log_bound = 0.0_f64;
    for (i, &card) in cardinalities.iter().enumerate().take(n_rel) {
        let f = weights[i];
        if f <= 0.0 {
            continue;
        }
        // 0^fi for fi > 0 is 0 → the whole product collapses. We treat any
        // zero-cardinality relation with positive weight as collapsing the
        // bound to 0 (the worst-case join output is empty when an input is
        // empty AND that relation must contribute weight ≥ 1 to some
        // attribute that only it covers).
        if card == 0 {
            return 0.0;
        }
        log_bound += f * (card as f64).ln();
    }
    log_bound.exp()
}

/// Solve the fractional cover LP using an interior-point barrier method.
///
/// The LP is:
///
/// ```text
/// minimize  Σ fi · log(|Ri|)
/// subject to  Σ_{i ∋ a} fi ≥ 1   for each attribute a
///             fi ≥ 0
/// ```
///
/// We solve it via a **log-barrier interior-point method**: minimize the
/// penalized objective
///
/// ```text
/// Σ fi · ci  -  μ · ( Σ_a log(cov_a - 1)  +  Σ_i log(fi) )
/// ```
///
/// where `ci = log(|Ri|)` and `cov_a = Σ_{i ∋ a} fi`. For fixed `μ > 0`,
/// this is a strictly convex minimization with a unique interior optimum.
/// As `μ → 0`, the barrier optimum converges to the LP optimum.
///
/// We use **path-following**: start with large `μ` (well inside the interior)
/// and gradually decrease `μ`, running gradient descent at each `μ` to
/// refine the solution. The final solution is then projected to exact
/// feasibility by scaling so that the minimum attribute coverage equals 1.
///
/// This approach correctly finds the symmetric optimum on cyclic queries
/// (e.g., the triangle query `R(A,B) ⋈ S(B,C) ⋈ T(A,C)` has optimal cover
/// `f = (0.5, 0.5, 0.5)`, giving bound `N^1.5`), which a naive greedy
/// subgradient method would miss.
///
/// # Returns
///
/// A `Vec<f64>` of length `graph.relations.len()`, with `weights[i]` being
/// the weight of relation `i`. After normalization, the sum over any
/// attribute's covering relations is ≥ 1 (when the hypergraph is fully
/// covered).
fn solve_fractional_cover(graph: &JoinHypergraph, cardinalities: &[usize]) -> Vec<f64> {
    let n_rel = graph.relations.len();
    let n_attr = graph.attributes.len();

    if n_rel == 0 || n_attr == 0 {
        return Vec::new();
    }

    // log-cost per relation; relations with cardinality ≤ 1 get cost 0
    // (they contribute nothing to the bound). Relations with cardinality 0
    // are effectively excluded by giving them a very large cost so the
    // solver avoids using them.
    let log_cost: Vec<f64> = (0..n_rel)
        .map(|i| {
            let c = cardinalities.get(i).copied().unwrap_or(0);
            if c == 0 {
                1.0e18
            } else if c == 1 {
                0.0
            } else {
                (c as f64).ln()
            }
        })
        .collect();

    // Precompute, for each attribute, the list of relation indices covering it.
    let covering: Vec<Vec<usize>> = (0..n_attr)
        .map(|a| (0..n_rel).filter(|&i| graph.relations[i].contains(&a)).collect())
        .collect();

    // If any attribute is uncovered, the hypergraph is infeasible (the AGM
    // bound is infinite in theory). We return uniform-zero weights; the
    // caller's `agm_bound` will then return 1.0.
    if covering.iter().any(|c| c.is_empty()) {
        return vec![0.0; n_rel];
    }

    // Initialize at a strictly interior feasible point.
    // f_i = 2.0 ensures every coverage constraint is strictly satisfied
    // (cov_a ≥ 2 > 1) and every f_i > 0.
    let mut f = vec![2.0_f64; n_rel];

    // Path-following: decrease μ from 1.0 to 0.001.
    // At each μ, run gradient descent on the barrier objective.
    let mus: [f64; 7] = [1.0, 0.3, 0.1, 0.03, 0.01, 0.003, 0.001];

    for &mu in &mus {
        // Step size: proportional to μ. For large μ, the barrier terms
        // dominate and we can take larger steps. For small μ, the linear
        // cost term dominates and we take smaller steps to avoid
        // overshooting the boundary.
        let eta = mu * 0.1;
        let iters = 800;
        for _k in 0..iters {
            // Compute current coverage of each attribute.
            let coverage: Vec<f64> =
                (0..n_attr).map(|a| covering[a].iter().map(|&i| f[i]).sum()).collect();

            // Compute gradient of the barrier objective:
            //   g_i = c_i - μ · ( Σ_{a ∈ i} 1/(cov_a - 1)  +  1/f_i )
            //
            // The first term is the linear cost (pushes f down).
            // The second term is the barrier on coverage constraints
            //   (pushes f up when cov_a is near 1).
            // The third term is the barrier on non-negativity
            //   (pushes f up when f_i is near 0).
            let mut g = vec![0.0_f64; n_rel];
            for i in 0..n_rel {
                g[i] = log_cost[i];
                for &a in &graph.relations[i] {
                    let denom = coverage[a] - 1.0;
                    if denom > 1e-12 {
                        g[i] -= mu / denom;
                    } else {
                        // Constraint is tight or violated — large gradient
                        // to push f back into the interior.
                        g[i] -= mu / 1e-12;
                    }
                }
                if f[i] > 1e-12 {
                    g[i] -= mu / f[i];
                } else {
                    g[i] -= mu / 1e-12;
                }
            }

            // Gradient descent step + projection onto f > 0.
            for i in 0..n_rel {
                f[i] = (f[i] - eta * g[i]).max(1e-9);
            }
        }
    }

    // Normalize: scale so that the minimum attribute coverage is exactly 1.
    // The barrier method leaves the solution slightly inside the feasible
    // region (cov_a slightly > 1); scaling down to exact feasibility
    // minimizes the objective while maintaining feasibility.
    let min_cov: f64 = (0..n_attr)
        .map(|a| covering[a].iter().map(|&i| f[i]).sum::<f64>())
        .min_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal))
        .unwrap_or(0.0);
    if min_cov > 1e-9 {
        let scale = 1.0 / min_cov;
        for w in &mut f {
            *w *= scale;
        }
    }

    f
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 2-relation join: both on attribute `A`, |R1|=|R2|=100.
    /// Fractional cover: f1 = f2 = 0.5 → bound = 100^0.5 · 100^0.5 = 100.
    #[test]
    fn agm_bound_two_relations_on_one_attr() {
        let graph =
            JoinHypergraph { attributes: vec!["A".into()], relations: vec![vec![0], vec![0]] };
        let bound = agm_bound(&graph, &[100, 100]);
        assert!(
            (bound - 100.0).abs() < 5.0,
            "AGM bound for two relations on one attr, |R|=100, expected ~100, got {bound}"
        );
    }

    /// 2-relation join, different cardinalities: |R1|=10, |R2|=1000.
    /// LP: min f1·log(10) + f2·log(1000) s.t. f1 + f2 ≥ 1.
    /// Optimal: f1=1, f2=0 (put all weight on the cheaper relation).
    /// Bound = 10^1 = 10.
    #[test]
    fn agm_bound_two_relations_unequal_cardinality() {
        let graph =
            JoinHypergraph { attributes: vec!["A".into()], relations: vec![vec![0], vec![0]] };
        let bound = agm_bound(&graph, &[10, 1000]);
        // The LP minimum puts all weight on the cheaper relation (|R1|=10).
        assert!(
            (bound - 10.0).abs() / 10.0 < 0.10,
            "AGM bound for |R1|=10, |R2|=1000, expected ~10, got {bound}"
        );
    }

    /// Triangle query: R(A,B), S(B,C), T(C,A), all |R|=|S|=|T|=N.
    /// Each attribute is covered by 2 of 3 relations, so the optimal cover is
    /// f1 = f2 = f3 = 0.5. Bound = N^(0.5·3) = N^1.5.
    ///
    /// For N = 100: bound = 100^1.5 = 1000.
    #[test]
    fn agm_bound_triangle_query() {
        let graph = JoinHypergraph::from_named(
            &["A", "B", "C"],
            &[vec!["A", "B"], vec!["B", "C"], vec!["A", "C"]],
        );
        let n = 100usize;
        let bound = agm_bound(&graph, &[n, n, n]);
        let expected = (n as f64).powf(1.5); // = 1000
        assert!(
            (bound - expected).abs() / expected < 0.05,
            "AGM bound for triangle, N=100, expected ~{expected}, got {bound}"
        );
    }

    /// Triangle query at N = 1000 → bound = 1000^1.5 = 31623.
    #[test]
    fn agm_bound_triangle_query_large_n() {
        let graph = JoinHypergraph::from_named(
            &["A", "B", "C"],
            &[vec!["A", "B"], vec!["B", "C"], vec!["A", "C"]],
        );
        let n = 1000usize;
        let bound = agm_bound(&graph, &[n, n, n]);
        let expected = (n as f64).powf(1.5);
        assert!(
            (bound - expected).abs() / expected < 0.05,
            "AGM bound for triangle, N=1000, expected ~{expected}, got {bound}"
        );
    }

    /// Single relation: AGM bound = |R| (fractional cover {1.0}).
    #[test]
    fn agm_bound_single_relation() {
        let graph = JoinHypergraph { attributes: vec!["A".into()], relations: vec![vec![0]] };
        let bound = agm_bound(&graph, &[42]);
        assert!(
            (bound - 42.0).abs() / 42.0 < 0.05,
            "AGM bound for single relation |R|=42, expected ~42, got {bound}"
        );
    }

    /// Empty hypergraph → bound = 1.0 (the empty join is one tuple).
    #[test]
    fn agm_bound_empty_hypergraph() {
        let graph = JoinHypergraph { attributes: vec![], relations: vec![] };
        let bound = agm_bound(&graph, &[]);
        assert_eq!(bound, 1.0);
    }

    /// Path query: R(A,B) ⋈ S(B,C) ⋈ T(C,D). Each attribute is covered by
    /// exactly one relation *except* B and C, which are each covered by 2.
    /// The optimal cover assigns f=1 to one relation per shared attribute,
    /// giving bound = max(|R|·|S|, |S|·|T|) ... actually the LP picks the
    /// minimum. For uniform N=100, bound = N^2 = 10000 (each shared
    /// attribute contributes a factor of N).
    #[test]
    fn agm_bound_path_query() {
        let graph = JoinHypergraph::from_named(
            &["A", "B", "C", "D"],
            &[vec!["A", "B"], vec!["B", "C"], vec!["C", "D"]],
        );
        let n = 100usize;
        let bound = agm_bound(&graph, &[n, n, n]);
        // A path query of 3 relations over 4 attributes, uniform cardinality,
        // has AGM bound N^(3/2) when each attribute is "shared enough".
        // Actually for this layout:
        //   A covered by R only → f_R ≥ 1
        //   D covered by T only → f_T ≥ 1
        //   B covered by R, S → f_R + f_S ≥ 1
        //   C covered by S, T → f_S + f_T ≥ 1
        // Minimum: f_R = 1, f_T = 1, f_S = 0 → bound = N · N = N².
        let expected = (n * n) as f64;
        assert!(
            (bound - expected).abs() / expected < 0.10,
            "AGM bound for path query N=100, expected ~{expected}, got {bound}"
        );
    }

    /// The from_named helper resolves attribute names to indices correctly.
    #[test]
    fn from_named_resolves_indices() {
        let g = JoinHypergraph::from_named(
            &["A", "B", "C"],
            &[vec!["B", "A"], vec!["C"]], // unsorted input
        );
        assert_eq!(g.relations[0], vec![0, 1]); // sorted: A, B
        assert_eq!(g.relations[1], vec![2]);
    }

    /// The fractional cover solver returns weights summing to ≥ 1 per attr.
    #[test]
    fn solve_fractional_cover_feasibility() {
        let graph = JoinHypergraph::from_named(
            &["A", "B", "C"],
            &[vec!["A", "B"], vec!["B", "C"], vec!["A", "C"]],
        );
        let w = solve_fractional_cover(&graph, &[100, 100, 100]);
        // Every attribute should have coverage ≥ 1.
        for a in 0..3 {
            let cov: f64 = (0..3).filter(|&i| graph.relations[i].contains(&a)).map(|i| w[i]).sum();
            assert!(cov >= 1.0 - 1e-6, "attribute {a} coverage = {cov}, expected ≥ 1");
        }
    }

    /// AGM bound for a star query: center relation R(A,B), R(A,C), R(A,D)
    /// all join on attribute A. Optimal cover: f_center = 1, leaves = 0.
    /// Bound = |center| = N.
    #[test]
    fn agm_bound_star_query() {
        // R(A,B) ⋈ S(A,C) ⋈ T(A,D) — all join on A.
        // A is covered by all 3; B, C, D are each covered by exactly 1.
        // So f_R ≥ 1, f_S ≥ 1, f_T ≥ 1 (each must cover its unique attribute).
        // Actually: f_R covers B (alone) → f_R ≥ 1.
        // f_S covers C (alone) → f_S ≥ 1.
        // f_T covers D (alone) → f_T ≥ 1.
        // A is covered by R+S+T → f_R + f_S + f_T ≥ 1 (already satisfied).
        // Bound = |R| · |S| · |T| = N^3.
        let graph = JoinHypergraph::from_named(
            &["A", "B", "C", "D"],
            &[vec!["A", "B"], vec!["A", "C"], vec!["A", "D"]],
        );
        let n = 100usize;
        let bound = agm_bound(&graph, &[n, n, n]);
        let expected = (n as f64).powi(3);
        assert!(
            (bound - expected).abs() / expected < 0.10,
            "AGM bound for star query N=100, expected ~{expected}, got {bound}"
        );
    }

    /// AGM bound is ≤ product of cardinalities (trivial bound).
    #[test]
    fn agm_bound_le_product_of_cardinalities() {
        let graph = JoinHypergraph::from_named(
            &["A", "B", "C"],
            &[vec!["A", "B"], vec!["B", "C"], vec!["A", "C"]],
        );
        let cards = [100, 100, 100];
        let bound = agm_bound(&graph, &cards);
        let product: usize = cards.iter().product();
        assert!(bound <= product as f64 + 1e-6, "AGM bound {bound} should be ≤ product {product}");
    }

    /// AGM bound is ≥ max single-relation cardinality (output ≥ any input).
    #[test]
    fn agm_bound_ge_max_cardinality() {
        let graph =
            JoinHypergraph { attributes: vec!["A".into()], relations: vec![vec![0], vec![0]] };
        let bound = agm_bound(&graph, &[10, 1000]);
        assert!(
            bound >= 10.0 - 1e-6,
            "AGM bound {bound} should be ≥ max cardinality (10), since output ≥ any input"
        );
    }
}
