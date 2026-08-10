//! Transaction control — SAVEPOINT, ROLLBACK TO, RELEASE.

use super::*;

impl QueryEngine {
    pub(crate) fn execute_savepoint(&mut self, name: String, start: &Instant) -> Result<QueryResult> {
        // Note: during execute_inner, the txn_manager.active field is
        // temporarily taken out (swapped). We can't check is_active()
        // here. Instead, we rely on the caller (execute) to only dispatch
        // to execute_inner when a txn is active. If we reach here without
        // a txn, the savepoint is simply created on the main catalog —
        // it won't be very useful, but it won't crash.
        // Deep-clone the current catalog as the savepoint state.
        let snapshot = crate::txn::clone_catalog(&self.catalog);
        self.savepoints.push((name, snapshot));
        let mut result = QueryResult::empty();
        result.elapsed_us = start.elapsed().as_micros() as u64;
        Ok(result)
    }

    /// Execute ROLLBACK TO <name>: restore the catalog to the named
    /// savepoint (Wave 69). All savepoints created after <name> are
    /// discarded.
    pub(crate) fn execute_rollback_to(&mut self, name: &str, start: &Instant) -> Result<QueryResult> {
        // Find the savepoint by name (search from the end — most recent first).
        let pos = self
            .savepoints
            .iter()
            .rposition(|(n, _)| n == name)
            .ok_or_else(|| Error::NotFound(format!("savepoint '{}'", name)))?;
        // Restore the catalog from the savepoint.
        let (_, snapshot) = &self.savepoints[pos];
        self.catalog = crate::txn::clone_catalog(snapshot);
        // Discard all savepoints after this one (they're no longer valid).
        self.savepoints.truncate(pos + 1);
        let mut result = QueryResult::empty();
        result.elapsed_us = start.elapsed().as_micros() as u64;
        Ok(result)
    }

    /// Execute RELEASE <name>: discard a savepoint (Wave 69).
    /// The savepoint is removed; ROLLBACK TO can no longer target it.
    pub(crate) fn execute_release_savepoint(&mut self, name: &str, start: &Instant) -> Result<QueryResult> {
        let pos = self
            .savepoints
            .iter()
            .rposition(|(n, _)| n == name)
            .ok_or_else(|| Error::NotFound(format!("savepoint '{}'", name)))?;
        // Remove the savepoint and all savepoints after it.
        self.savepoints.truncate(pos);
        let mut result = QueryResult::empty();
        result.elapsed_us = start.elapsed().as_micros() as u64;
        Ok(result)
    }
}
