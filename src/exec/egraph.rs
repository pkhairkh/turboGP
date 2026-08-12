//! Minimal e-graph for arithmetic expression optimization.
//!
//! Implements the e-graph *concept* natively in Rust (no `egg` dependency).
//! An e-graph represents expressions as a set of e-classes, where each e-class
//! contains a set of e-nodes that are all equivalent. Rewrite rules add new
//! equivalences; the e-graph maintains the transitive closure. A cost-based
//! extractor picks the cheapest representation from each e-class.
//!
//! Used by `expr_eval.rs` to optimize `CompiledNode` trees:
//!   1. Lower `CompiledNode` to e-graph (`add_node`)
//!   2. Saturate with rewrite rules (`saturate`)
//!   3. Extract the cheapest form (`extract`)
//!   4. Rebuild `CompiledNode` from the extracted e-node
//!
//! ## Supported rewrites
//!
//! - Identity: `x + 0 -> x`, `x * 1 -> x`, `x - 0 -> x`
//! - Zero: `x * 0 -> 0`, `x - x -> 0`
//! - Strength reduction: `x * 2 -> x + x`
//! - Distributivity: `a * (b + c) <-> a * b + a * c`
//! - Constant folding: `lit + lit -> lit`, `lit * lit -> lit`, etc.
//!
//! ## Cost model
//!
//! Default cost function assigns instruction-latency-based costs:
//! `Lit=1`, `Col=1`, `Add=3`, `Sub=3`, `Mul=5`.

use std::collections::HashMap;

/// Identifier for an e-class (a set of equivalent e-nodes).
pub type EClassId = u32;

/// Binary operation kinds supported by the e-graph.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum BinOpKind {
    Add,
    Sub,
    Mul,
    Div,
}

impl BinOpKind {
    pub fn from_char(c: char) -> Option<Self> {
        match c {
            '+' => Some(BinOpKind::Add),
            '-' => Some(BinOpKind::Sub),
            '*' => Some(BinOpKind::Mul),
            '/' => Some(BinOpKind::Div),
            _ => None,
        }
    }
    pub fn to_char(self) -> char {
        match self {
            BinOpKind::Add => '+',
            BinOpKind::Sub => '-',
            BinOpKind::Mul => '*',
            BinOpKind::Div => '/',
        }
    }
}

/// An e-node: a single node in the e-graph.
///
/// `BinOp` references its children by `EClassId`, so when e-classes merge,
/// all parents of any e-node in the merged class automatically see the new
/// equivalence.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum ENode {
    /// A floating-point literal (stored as `f64::to_bits`).
    /// Integer literals also stored here (with small magnitude).
    Lit(u64),
    /// A column reference (resolved to column index at lower time).
    Col(usize),
    /// A binary operation: `op(left_eclass, right_eclass)`.
    BinOp {
        op: BinOpKind,
        left: EClassId,
        right: EClassId,
    },
    /// Fused multiply-add: `a * b + c`.
    ///
    /// Semantically equivalent to `Add(Mul(a, b), c)` but cheaper on hardware
    /// with FMA support (1 FMA instruction vs 1 MUL + 1 ADD). The e-graph's
    /// rewrite rule `Add(Mul(a, b), c) -> Fma(a, b, c)` adds this form, and
    /// the cost function (Fma=4 vs Mul+Add=8) picks it.
    Fma {
        a: EClassId,
        b: EClassId,
        c: EClassId,
    },
}

impl ENode {
    /// Returns the child e-class IDs of this e-node (empty for leaves).
    pub fn children(&self) -> Vec<EClassId> {
        match self {
            ENode::Lit(_) | ENode::Col(_) => Vec::new(),
            ENode::BinOp { left, right, .. } => vec![*left, *right],
            ENode::Fma { a, b, c } => vec![*a, *b, *c],
        }
    }
}

/// An e-class: a set of e-nodes that are all equivalent.
#[derive(Debug, Clone, Default)]
pub struct EClass {
    pub nodes: Vec<ENode>,
    /// Cached best-cost extraction (memoized). `None` means "not computed".
    pub best: Option<(u64, ENode)>,
}

/// The e-graph data structure.
pub struct EGraph {
    pub classes: HashMap<EClassId, EClass>,
    pub memo: HashMap<ENode, EClassId>,
    uf: UnionFind,
    next_id: EClassId,
}

impl EGraph {
    pub fn new() -> Self {
        Self {
            classes: HashMap::new(),
            memo: HashMap::new(),
            uf: UnionFind::new(),
            next_id: 0,
        }
    }

    /// Add an e-node to the e-graph. If the e-node already exists (via memo),
    /// returns the existing e-class ID. Otherwise creates a new e-class.
    pub fn add(&mut self, node: ENode) -> EClassId {
        // Check memo first (hash-cons).
        if let Some(&id) = self.memo.get(&node) {
            return self.uf.find(id);
        }
        let id = self.next_id;
        self.next_id += 1;
        self.uf.add(id);
        self.classes.insert(
            id,
            EClass {
                nodes: vec![node.clone()],
                best: None,
            },
        );
        self.memo.insert(node, id);
        id
    }

    /// Find the canonical e-class ID for a given e-class ID (after merges).
    pub fn find(&self, id: EClassId) -> EClassId {
        self.uf.find(id)
    }

    /// Merge two e-classes. After merging, both e-class IDs refer to the
    /// same canonical e-class. The merged class contains the union of both
    /// e-node sets.
    pub fn merge(&mut self, a: EClassId, b: EClassId) -> EClassId {
        let a = self.uf.find(a);
        let b = self.uf.find(b);
        if a == b {
            return a;
        }
        let b_class = match self.classes.remove(&b) {
            Some(c) => c,
            None => return a,
        };
        let a_class = self.classes.get_mut(&a).expect("a class exists");
        for node in &b_class.nodes {
            self.memo.insert(node.clone(), a);
            a_class.nodes.push(node.clone());
        }
        a_class.best = None; // invalidate cache
        self.uf.union(a, b);
        a
    }

    /// Apply rewrite rules until no new merges happen or `budget` iterations
    /// are reached.
    ///
    /// Each rule returns `Some(EClassId)` indicating "the matched e-node's
    /// e-class should be merged with this e-class ID". The rule may first
    /// add a new e-node to the e-graph (e.g., for constant folding) and
    /// return the new e-class ID.
    pub fn saturate<F>(&mut self, mut apply_rule: F, budget: usize)
    where
        F: FnMut(&ENode, &mut EGraph) -> Option<EClassId>,
    {
        for _ in 0..budget {
            let mut merges: Vec<(EClassId, EClassId)> = Vec::new();
            let class_ids: Vec<EClassId> = self.classes.keys().copied().collect();
            for cid in class_ids {
                let cid = self.uf.find(cid);
                let nodes: Vec<ENode> = self
                    .classes
                    .get(&cid)
                    .map(|c| c.nodes.clone())
                    .unwrap_or_default();
                for node in &nodes {
                    if let Some(target) = apply_rule(node, self) {
                        let target = self.uf.find(target);
                        if target != cid {
                            merges.push((cid, target));
                        }
                    }
                }
            }
            if merges.is_empty() {
                break;
            }
            for (a, b) in merges {
                self.merge(a, b);
            }
        }
    }

    /// Extract the cheapest e-node from an e-class using a cost function.
    ///
    /// The cost function takes the e-node and the cost of its left and right
    /// children, and returns the total cost. Lower is better.
    pub fn extract(&mut self, id: EClassId, cost_fn: &dyn Fn(&ENode, u64, u64) -> u64) -> ENode {
        let mut visiting: std::collections::HashSet<EClassId> = std::collections::HashSet::new();
        self.extract_inner(id, cost_fn, &mut visiting)
    }

    fn extract_inner(
        &mut self,
        id: EClassId,
        cost_fn: &dyn Fn(&ENode, u64, u64) -> u64,
        visiting: &mut std::collections::HashSet<EClassId>,
    ) -> ENode {
        let id = self.uf.find(id);
        if let Some((_, node)) = self.classes.get(&id).and_then(|c| c.best.clone()) {
            return node;
        }
        // Cycle break: if we're already extracting this e-class, return the
        // first e-node (avoid infinite recursion through merged cycles).
        if !visiting.insert(id) {
            // Return the first e-node in the class (or Lit(0) if empty).
            return self
                .classes
                .get(&id)
                .and_then(|c| c.nodes.first().cloned())
                .unwrap_or(ENode::Lit(0));
        }
        let nodes: Vec<ENode> = self
            .classes
            .get(&id)
            .map(|c| c.nodes.clone())
            .unwrap_or_default();
        let mut best_cost = u64::MAX;
        let mut best_node: Option<ENode> = None;
        for node in &nodes {
            // Compute child costs. For Fma, we pack a+b into left_cost and
            // c into right_cost (the cost function adds left+right+self).
            let (lc, rc) = match node {
                ENode::Lit(_) | ENode::Col(_) => (0u64, 0u64),
                ENode::BinOp { left, right, .. } => {
                    let l = self.node_cost(*left, cost_fn, visiting);
                    let r = self.node_cost(*right, cost_fn, visiting);
                    (l, r)
                }
                ENode::Fma { a, b, c } => {
                    let ca = self.node_cost(*a, cost_fn, visiting);
                    let cb = self.node_cost(*b, cost_fn, visiting);
                    let cc = self.node_cost(*c, cost_fn, visiting);
                    // Pack a+b into left, c into right.
                    (ca.saturating_add(cb), cc)
                }
            };
            // Skip cyclic nodes: if either child has u64::MAX cost, the
            // node is self-referential. Picking it would cause infinite
            // recursion in extract_from_egraph.
            if lc == u64::MAX || rc == u64::MAX {
                continue;
            }
            let total = cost_fn(node, lc, rc);
            if total < best_cost {
                best_cost = total;
                best_node = Some(node.clone());
            }
        }
        let node = best_node.unwrap_or(ENode::Lit(0));
        if let Some(class) = self.classes.get_mut(&id) {
            class.best = Some((best_cost, node.clone()));
        }
        node
    }

    fn node_cost(
        &mut self,
        id: EClassId,
        cost_fn: &dyn Fn(&ENode, u64, u64) -> u64,
        visiting: &mut std::collections::HashSet<EClassId>,
    ) -> u64 {
        let id = self.uf.find(id);
        if let Some((cost, _)) = self.classes.get(&id).and_then(|c| c.best.clone()) {
            return cost;
        }
        let _ = self.extract_inner(id, cost_fn, visiting);
        // If extract_inner didn't cache a best (e.g., due to a cycle),
        // return u64::MAX so the cost function never picks a cyclic form.
        // Previously this returned 0, making cyclic nodes look "free" —
        // which caused the extractor to pick self-referential forms like
        // Mul(E, Lit(1)) where E contains Mul(E, Lit(1)) itself.
        self.classes
            .get(&id)
            .and_then(|c| c.best.clone())
            .map(|(c, _)| c)
            .unwrap_or(u64::MAX)
    }

    /// Helper: check if an e-class contains a `Lit(0)` (i.e. f64::to_bits(0.0) = 0).
    pub fn class_contains_lit_zero(&self, id: EClassId) -> bool {
        let id = self.find(id);
        if let Some(class) = self.classes.get(&id) {
            for node in &class.nodes {
                if let ENode::Lit(v) = node {
                    if *v == 0 {
                        return true;
                    }
                }
            }
        }
        false
    }

    /// Helper: check if an e-class contains a `Lit(1.0)` (f64::to_bits(1.0) = 4607182418800017408).
    pub fn class_contains_lit_one(&self, id: EClassId) -> bool {
        let id = self.find(id);
        if let Some(class) = self.classes.get(&id) {
            for node in &class.nodes {
                if let ENode::Lit(v) = node {
                    if f64::from_bits(*v) == 1.0 {
                        return true;
                    }
                }
            }
        }
        false
    }

    /// Helper: get the lit value if the e-class contains a single Lit.
    pub fn class_lit_value(&self, id: EClassId) -> Option<u64> {
        let id = self.find(id);
        let class = self.classes.get(&id)?;
        for node in &class.nodes {
            if let ENode::Lit(v) = node {
                return Some(*v);
            }
        }
        None
    }
}

impl Default for EGraph {
    fn default() -> Self {
        Self::new()
    }
}

// ---------------------------------------------------------------------------
// Union-Find data structure for e-class merging
// ---------------------------------------------------------------------------

struct UnionFind {
    parent: HashMap<EClassId, EClassId>,
    rank: HashMap<EClassId, u32>,
}

impl UnionFind {
    fn new() -> Self {
        Self {
            parent: HashMap::new(),
            rank: HashMap::new(),
        }
    }

    fn add(&mut self, id: EClassId) {
        self.parent.entry(id).or_insert(id);
        self.rank.entry(id).or_insert(0);
    }

    fn find(&self, mut id: EClassId) -> EClassId {
        while let Some(&p) = self.parent.get(&id) {
            if p == id {
                return id;
            }
            id = p;
        }
        id
    }

    fn union(&mut self, a: EClassId, b: EClassId) {
        let root_a = self.find(a);
        let root_b = self.find(b);
        if root_a == root_b {
            return;
        }
        let rank_a = *self.rank.get(&root_a).unwrap_or(&0);
        let rank_b = *self.rank.get(&root_b).unwrap_or(&0);
        let (new_root, new_child) = if rank_a < rank_b {
            (root_b, root_a)
        } else {
            (root_a, root_b)
        };
        self.parent.insert(new_child, new_root);
        if rank_a == rank_b {
            *self.rank.entry(new_root).or_insert(0) += 1;
        }
    }
}

// ---------------------------------------------------------------------------
// Standard rewrite rules + cost function
// ---------------------------------------------------------------------------

/// Apply all standard rewrite rules to an e-node, returning an e-class ID
/// to merge with. Call this in `saturate`.
pub fn apply_standard_rules(node: &ENode, eg: &mut EGraph) -> Option<EClassId> {
    // Identity: x + 0 -> x, x * 1 -> x, x - 0 -> x
    if let ENode::BinOp { op, left, right } = node {
        let right = eg.find(*right);
        let left = eg.find(*left);
        match op {
            BinOpKind::Add if eg.class_contains_lit_zero(right) => return Some(left),
            BinOpKind::Sub if eg.class_contains_lit_zero(right) => return Some(left),
            BinOpKind::Mul if eg.class_contains_lit_one(right) => return Some(left),
            BinOpKind::Mul if eg.class_contains_lit_one(left) => return Some(right),
            BinOpKind::Mul if eg.class_contains_lit_zero(right) || eg.class_contains_lit_zero(left) => {
                // x * 0 -> 0
                let zero = eg.add(ENode::Lit(0));
                return Some(zero);
            }
            _ => {}
        }
        // x - x -> 0 (both children are the same e-class)
        if *op == BinOpKind::Sub && left == right {
            let zero = eg.add(ENode::Lit(0));
            return Some(zero);
        }
        // Strength reduction: x * 2 -> x + x
        if *op == BinOpKind::Mul {
            if let Some(rv) = eg.class_lit_value(right) {
                if f64::from_bits(rv) == 2.0 {
                    let x_plus_x = eg.add(ENode::BinOp {
                        op: BinOpKind::Add,
                        left,
                        right: left,
                    });
                    return Some(x_plus_x);
                }
            }
        }
        // Constant folding: lit OP lit -> lit
        let lv = eg.class_lit_value(left);
        let rv = eg.class_lit_value(right);
        if let (Some(lv), Some(rv)) = (lv, rv) {
            let lf = f64::from_bits(lv);
            let rf = f64::from_bits(rv);
            let result = match op {
                BinOpKind::Add => lf + rf,
                BinOpKind::Sub => lf - rf,
                BinOpKind::Mul => lf * rf,
                BinOpKind::Div => {
                    if rf == 0.0 {
                        return None;
                    }
                    lf / rf
                }
            };
            let lit = eg.add(ENode::Lit(result.to_bits()));
            return Some(lit);
        }
        // FMA pattern: a*b + c -> Fma(a, b, c)
        // Fused multiply-add is 1 instruction on hardware with FMA support
        // (Zen 5: 4-cycle latency, 2/cycle throughput). Cheaper than separate
        // MUL (5 cycles) + ADD (3 cycles) = 8 cycles.
        if *op == BinOpKind::Add {
            // Check if left child is a Mul: Add(Mul(a, b), c) -> Fma(a, b, c)
            if let Some(left_class) = eg.classes.get(&left) {
                for left_node in left_class.nodes.clone() {
                    if let ENode::BinOp { op: BinOpKind::Mul, left: a, right: b } = left_node {
                        let fma = eg.add(ENode::Fma { a, b, c: right });
                        return Some(fma);
                    }
                }
            }
            // Check if right child is a Mul: Add(c, Mul(a, b)) -> Fma(a, b, c)
            if let Some(right_class) = eg.classes.get(&right) {
                for right_node in right_class.nodes.clone() {
                    if let ENode::BinOp { op: BinOpKind::Mul, left: a, right: b } = right_node {
                        let fma = eg.add(ENode::Fma { a, b, c: left });
                        return Some(fma);
                    }
                }
            }
        }

        // Distributivity: a * (b + c) -> a*b + a*c
        //
        // DISABLED: distributivity creates cycles through the identity rule
        // (x*1 → x). When `a * (1 + c)` is expanded to `a*1 + a*c`, identity
        // rewrites `a*1 → a`, merging the Mul e-class into a's e-class. If
        // `a` is the left child of the Mul, this creates a self-referential
        // e-class (a contains Mul(a, 1) which references a). The extractor's
        // cycle-break doesn't handle this correctly — it returns Lit(0) as
        // a fallback, causing the entire expression to collapse to 0.
        //
        // TODO (W3-T3): implement proper cyclic e-class extraction (Tarjan's
        // SCC + topological cost computation) and re-enable distributivity.
        // For now, the other rules (identity, zero, strength reduction,
        // constant folding) provide real optimization value without cycles.
        // #[cfg(any())]  // compile-time disable
        if false {
            if let Some(right_class) = eg.classes.get(&right) {
                for right_node in right_class.nodes.clone() {
                    if let ENode::BinOp { op: BinOpKind::Add, left: b, right: c } = right_node {
                        let ab = eg.add(ENode::BinOp {
                            op: BinOpKind::Mul,
                            left,
                            right: b,
                        });
                        let ac = eg.add(ENode::BinOp {
                            op: BinOpKind::Mul,
                            left,
                            right: c,
                        });
                        let ab_plus_ac = eg.add(ENode::BinOp {
                            op: BinOpKind::Add,
                            left: ab,
                            right: ac,
                        });
                        return Some(ab_plus_ac);
                    }
                }
            }
        }
    }
    None
}

/// Default cost function based on instruction latencies (Zen 5).
/// Returns the cost of an e-node given its children's costs.
pub fn default_cost_fn(node: &ENode, left_cost: u64, right_cost: u64) -> u64 {
    let self_cost = match node {
        ENode::Lit(_) => 1,
        ENode::Col(_) => 1,
        ENode::BinOp { op, .. } => match op {
            BinOpKind::Add => 3,
            BinOpKind::Sub => 3,
            BinOpKind::Mul => 5,
            BinOpKind::Div => 20,
        },
        // FMA: a*b + c in 1 instruction (4-cycle latency on Zen 5).
        // Cheaper than Add(Mul, c) = 3 + 5 = 8. The third child's cost is
        // passed via right_cost (we pack a/b into left_cost, c into right_cost).
        ENode::Fma { .. } => 4,
    };
    self_cost + left_cost + right_cost
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_add_and_hash_cons() {
        let mut eg = EGraph::new();
        let a = eg.add(ENode::Col(0));
        let b = eg.add(ENode::Col(0));
        assert_eq!(a, b, "same ENode should hash-cons to same EClassId");
    }

    #[test]
    fn test_merge() {
        let mut eg = EGraph::new();
        let a = eg.add(ENode::Col(0));
        let b = eg.add(ENode::Col(1));
        let merged = eg.merge(a, b);
        assert_eq!(eg.find(a), eg.find(b));
        assert_eq!(eg.find(a), merged);
    }

    #[test]
    fn test_constant_fold() {
        // 2 + 3 -> 5
        let mut eg = EGraph::new();
        let two = eg.add(ENode::Lit(2.0f64.to_bits()));
        let three = eg.add(ENode::Lit(3.0f64.to_bits()));
        let sum = eg.add(ENode::BinOp {
            op: BinOpKind::Add,
            left: two,
            right: three,
        });
        eg.saturate(apply_standard_rules, 10);
        // After saturate, the sum's e-class should contain Lit(5).
        let sum_canon = eg.find(sum);
        let class = eg.classes.get(&sum_canon).unwrap();
        let has_five = class
            .nodes
            .iter()
            .any(|n| matches!(n, ENode::Lit(v) if f64::from_bits(*v) == 5.0));
        assert!(has_five, "e-class should contain Lit(5.0) after constant fold");
    }

    #[test]
    fn test_identity_add_zero() {
        // x + 0 -> x
        let mut eg = EGraph::new();
        let x = eg.add(ENode::Col(0));
        let zero = eg.add(ENode::Lit(0)); // 0 is f64::to_bits(0.0)
        let x_plus_zero = eg.add(ENode::BinOp {
            op: BinOpKind::Add,
            left: x,
            right: zero,
        });
        eg.saturate(apply_standard_rules, 10);
        // After saturate, x_plus_zero's e-class should be merged with x's.
        assert_eq!(eg.find(x_plus_zero), eg.find(x));
    }

    #[test]
    fn test_identity_mul_one() {
        // x * 1 -> x
        let mut eg = EGraph::new();
        let x = eg.add(ENode::Col(0));
        let one = eg.add(ENode::Lit(1.0f64.to_bits()));
        let x_mul_one = eg.add(ENode::BinOp {
            op: BinOpKind::Mul,
            left: x,
            right: one,
        });
        eg.saturate(apply_standard_rules, 10);
        assert_eq!(eg.find(x_mul_one), eg.find(x));
    }

    #[test]
    fn test_zero_mul() {
        // x * 0 -> 0
        let mut eg = EGraph::new();
        let x = eg.add(ENode::Col(0));
        let zero = eg.add(ENode::Lit(0));
        let x_mul_zero = eg.add(ENode::BinOp {
            op: BinOpKind::Mul,
            left: x,
            right: zero,
        });
        eg.saturate(apply_standard_rules, 10);
        // After saturate, x_mul_zero's e-class should contain Lit(0).
        let canon = eg.find(x_mul_zero);
        let class = eg.classes.get(&canon).unwrap();
        let has_zero = class
            .nodes
            .iter()
            .any(|n| matches!(n, ENode::Lit(0)));
        assert!(has_zero, "x * 0 should fold to Lit(0)");
    }

    #[test]
    fn test_strength_reduction_x_mul_2() {
        // x * 2 -> x + x
        let mut eg = EGraph::new();
        let x = eg.add(ENode::Col(0));
        let two = eg.add(ENode::Lit(2.0f64.to_bits()));
        let x_mul_two = eg.add(ENode::BinOp {
            op: BinOpKind::Mul,
            left: x,
            right: two,
        });
        eg.saturate(apply_standard_rules, 10);
        // After saturate, the e-class should contain BinOp{Add, x, x}.
        let canon = eg.find(x_mul_two);
        let class = eg.classes.get(&canon).unwrap();
        let has_add = class.nodes.iter().any(|n| {
            matches!(n, ENode::BinOp { op: BinOpKind::Add, .. })
        });
        assert!(has_add, "x * 2 should be rewritten to x + x");
    }

    #[test]
    fn test_extract_picks_cheaper() {
        // x + 0 should extract to x (Col, cost=1) not BinOp (cost=4).
        let mut eg = EGraph::new();
        let x = eg.add(ENode::Col(0));
        let zero = eg.add(ENode::Lit(0));
        let x_plus_zero = eg.add(ENode::BinOp {
            op: BinOpKind::Add,
            left: x,
            right: zero,
        });
        eg.saturate(apply_standard_rules, 10);
        let extracted = eg.extract(x_plus_zero, &default_cost_fn);
        assert!(
            matches!(extracted, ENode::Col(0)),
            "extract should pick Col(0) (cost 1) over BinOp (cost 4); got {:?}",
            extracted
        );
    }

    #[test]
    fn test_fma_picked_over_mul_add() {
        // a * b + c should be rewritten to Fma(a, b, c).
        // Fma cost = 4 + 1 + 1 + 1 = 7 (Fma=4, a=Col=1, b=Col=1, c=Col=1).
        // Add(Mul(a, b), c) cost = 3 + (5 + 1 + 1) + 1 = 11.
        // Extractor should pick Fma (7 < 11).
        let mut eg = EGraph::new();
        let a = eg.add(ENode::Col(0));
        let b = eg.add(ENode::Col(1));
        let c = eg.add(ENode::Col(2));
        let ab = eg.add(ENode::BinOp { op: BinOpKind::Mul, left: a, right: b });
        let sum = eg.add(ENode::BinOp { op: BinOpKind::Add, left: ab, right: c });
        eg.saturate(apply_standard_rules, 10);
        let extracted = eg.extract(sum, &default_cost_fn);
        assert!(
            matches!(extracted, ENode::Fma { .. }),
            "extract should pick Fma over Add(Mul, c); got {:?}",
            extracted
        );
    }

    #[test]
    fn test_fma_cost_cheaper_than_mul_add() {
        // Verify the cost function assigns Fma=4 (cheaper than Mul=5 + Add=3 = 8).
        let fma_cost = default_cost_fn(
            &ENode::Fma { a: 0, b: 1, c: 2 },
            2, // left_cost (a + b)
            1, // right_cost (c)
        );
        // Fma self_cost = 4, total = 4 + 2 + 1 = 7.
        assert_eq!(fma_cost, 7);

        let mul_add_cost = default_cost_fn(
            &ENode::BinOp { op: BinOpKind::Add, left: 0, right: 1 },
            6, // left_cost = Mul(a, b) = 5 + 1 + 1 = 7... but we pass 6 for the test
            1, // right_cost = c
        );
        // Add self_cost = 3, total = 3 + 6 + 1 = 10.
        assert_eq!(mul_add_cost, 10);
        assert!(fma_cost < mul_add_cost, "Fma ({}) should be cheaper than Mul+Add ({})", fma_cost, mul_add_cost);
    }

    #[test]
    fn test_extract_constant_fold() {
        // 2 + 3 should extract to Lit(5) (cost 1) not BinOp (cost 7).
        let mut eg = EGraph::new();
        let two = eg.add(ENode::Lit(2.0f64.to_bits()));
        let three = eg.add(ENode::Lit(3.0f64.to_bits()));
        let sum = eg.add(ENode::BinOp {
            op: BinOpKind::Add,
            left: two,
            right: three,
        });
        eg.saturate(apply_standard_rules, 10);
        let extracted = eg.extract(sum, &default_cost_fn);
        assert!(
            matches!(extracted, ENode::Lit(v) if f64::from_bits(v) == 5.0),
            "extract should pick Lit(5.0) (cost 1) over BinOp (cost 7); got {:?}",
            extracted
        );
    }
}
