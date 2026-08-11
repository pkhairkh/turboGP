# Integration Debt Log

This file documents debt items that could not be fully resolved during the
three-branch integration. Each entry lists the debt ID, what was expected,
what was actually delivered, the current workaround, and recommended next
steps.

---

## Debt-3.2: UNION ALL parser hack (`split_union_all`)

**Status:** PARTIALLY RESOLVED — formal parser has `SetQuery::UnionAll`, but
engine lacks `execute_select_query(parsed)` path.

**Expected:** Agent A adds `SetQuery::Union(left, right)` to the formal
parser; `execute_inner()` handles `SetQuery::Union` by executing both sides
and concatenating; `split_union_all()` deleted from `helpers.rs`.

**Delivered:** Agent A added `SetQuery::UnionAll(Box<SetQuery>, Box<SetQuery>)`
to `src/sql/parser.rs` (along with `Union`, `Intersect`, `Except`). The
formal parser (`parse_set()`) correctly parses UNION ALL queries.

**Current workaround:** The `split_union_all()` string hack in
`src/engine/helpers.rs` is **still in use** because the engine's execution
path takes SQL strings (not parsed ASTs). Removing the hack requires adding
an `execute_select_query(&mut self, query: &SelectQuery) -> Result<QueryResult>`
method to `QueryEngine` — a significant refactor that touches the executor,
planner, and kernel dispatch paths.

**Recommended next step:** Add `QueryEngine::execute_select_query(&SelectQuery)`
that executes a parsed `SelectQuery` directly (bypassing re-parse). Then
replace `split_union_all()` in `execute_inner()` with:
```rust
let tokens = crate::sql::lexer::tokenize(sql)?;
match crate::sql::parser::parse_set(tokens)? {
    SetQuery::UnionAll(left, right) => {
        let left_result = self.execute_select_query(left.as_select().unwrap())?;
        let right_result = self.execute_select_query(right.as_select().unwrap())?;
        return Ok(concatenate_results(left_result, right_result, start));
    }
    _ => { /* fall through */ }
}
```

---

## Debt-3.3: MERGE parser hack (`parse_merge`)

**Status:** NOT RESOLVED — Agent A did not add MERGE to the formal parser.

**Expected:** Agent A adds MERGE to the formal parser; `execute_inner()`
dispatches `StatementKind::Merge` to a formal MERGE AST handler.

**Delivered:** Agent A's branch does not modify `src/sql/parser.rs` to add
MERGE support. The `classify_statement()` function in `src/engine/dispatch.rs`
correctly returns `StatementKind::Merge` for MERGE statements, but there is
no formal MERGE AST to dispatch to.

**Current workaround:** The string-based `parse_merge()` hack in
`src/engine/helpers.rs` is **still in use**. It parses MERGE by
string-scanning and returns a `Merge` struct.

**Recommended next step:** Agent A should add a `MergeStmt` AST to
`src/sql/ast.rs` and a `parse_merge()` function to `src/sql/parser.rs` that
returns the formal AST. Then `execute_inner()` can dispatch
`StatementKind::Merge` to `execute_merge_ast(merge_stmt)`.

---

## Debt-3.4: PIVOT/UNPIVOT parser hack (`parse_pivot_clause`)

**Status:** NOT RESOLVED — Agent A did not add PIVOT/UNPIVOT to the formal
parser.

**Expected:** Agent A adds PIVOT/UNPIVOT to the formal parser;
`execute_inner()` handles the parsed `PivotSpec` directly.

**Delivered:** Agent A's branch does not add PIVOT/UNPIVOT parsing to
`src/sql/parser.rs` or `src/sql/ast.rs`.

**Current workaround:** The string-based `parse_pivot_clause()` and
`strip_pivot_clause()` hacks in `src/engine/helpers.rs` are **still in use**.

**Recommended next step:** Agent A should add a `PivotSpec` AST to
`src/sql/ast.rs` and PIVOT/UNPIVOT parsing to `src/sql/parser.rs`.

---

## Summary

| Debt ID | Status | Reason |
|---------|--------|--------|
| debt-2.3 (Catalog RwLock) | DEFERRED | Not blocking; QueryEngine-level RwLock works |
| debt-3.2 (UNION ALL) | PARTIAL | Parser has `SetQuery::UnionAll`; engine lacks `execute_select_query()` |
| debt-3.3 (MERGE) | NOT RESOLVED | Agent A didn't add MERGE parser |
| debt-3.4 (PIVOT) | NOT RESOLVED | Agent A didn't add PIVOT parser |
| debt-4.1 (begin_with_isolation) | RESOLVED | Agent B implemented; compat wrappers added |
| debt-4.2 (row_versions) | RESOLVED | Agent B's MvccTable + RowVersion available |
| debt-4.3 (vacuum) | RESOLVED | Agent B's MvccTxnManager::vacuum exists |
| debt-5.2 (append_and_sync) | RESOLVED | Agent B implemented; engine uses it |
| debt-5.3 (set_streamer) | RESOLVED | Agent B's WalStreamSink trait + set_stream_sink |
| debt-5.4 (on_become_leader) | RESOLVED | Agent B implemented |
| debt-6.3 (WAL timestamps) | PARTIAL | WalRecord has no timestamp_us field; PITR uses proxy |
| debt-6.x (list_tables bug) | RESOLVED | Agent B fixed list_tables to read from catalog directly |
| debt-6.2/6.3/6.5 (openraft) | DEFERRED | Requires async runtime (tokio) refactor — see section below |

---

## Debt: openraft integration (Wave 6 Tasks 6.2, 6.3, 6.5)

**Status:** DEFERRED — openraft requires a full async runtime (tokio)
refactor that is out of scope for this hardening programme.

**What was done instead:**
- Task 6.1: Synchronous replication mode (flush-based) — implemented.
- Task 6.4: LSN-based replica resume — implemented.
- The existing minimal `RaftNode` (hand-rolled, single-node election)
  remains. `enable_raft` creates a RaftNode and calls `on_become_leader`
  which connects WalStreamers to followers.

**What openraft would add:**
- Real multi-node leader election with term-based voting.
- Log replication with majority quorum.
- Automatic failover (leader dies → new leader elected within seconds).
- Network partition tolerance.

**Recommended next step:**
Migrate the engine to async (tokio), then replace the stub RaftNode with
`openraft::Raft`. This is a significant refactor (the entire server layer
needs to become async) and should be a separate workstream.
