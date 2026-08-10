//! **WIRED INTO SQL EXECUTION (Wave 53, fixed Wave 56d)** — this module is
//! reachable through `QueryEngine::execute()` via `parse_for_system_time` in
//! `engine/mod.rs`. Wave 56d added DDL support: `CREATE TABLE ... WITH
//! (SYSTEM_VERSIONING = ON)` now registers the table in `self.temporals`,
//! and `execute_insert` / `execute_update` / `execute_delete` sync changes
//! to the `TemporalTable` sidecar so `FOR SYSTEM_TIME AS OF <ts>` returns
//! the correct historical state.
//! # Temporal tables (Wave 11).
//!
//! Implements system-versioned temporal tables: every UPDATE/DELETE on a
//! temporal table writes the prior row to a history table with ValidFrom
//! and ValidTo timestamps. Queries can use FOR SYSTEM_TIME AS OF, BETWEEN,
//! CONTAINED IN, and ALL to query historical states.

use crate::engine::{QueryResult, ResultColumn};
use std::time::{SystemTime, UNIX_EPOCH};

/// A row in the history table: the original column values plus ValidFrom
/// and ValidTo timestamps (as u64 epoch millis).
#[derive(Debug, Clone)]
pub struct HistoryRow {
    pub values: Vec<u64>,
    pub valid_from: u64,
    pub valid_to: u64,
}

/// A temporal table manager: tracks the current rows and the history.
pub struct TemporalTable {
    /// Column names (same for current and history).
    pub column_names: Vec<String>,
    /// Current rows: each row is a Vec<u64> of column values + valid_from timestamp.
    pub current_rows: Vec<(Vec<u64>, u64)>,
    /// History rows: prior versions of rows that were updated/deleted.
    pub history: Vec<HistoryRow>,
    /// The timestamp of the last modification (for ValidFrom/ValidTo).
    pub last_modified: u64,
}

impl TemporalTable {
    /// Create a new temporal table with the given column names.
    pub fn new(column_names: Vec<String>) -> Self {
        Self {
            column_names,
            current_rows: Vec::new(),
            history: Vec::new(),
            last_modified: now_millis(),
        }
    }

    /// Insert a new row. The ValidFrom is set to now; ValidTo is set to
    /// u64::MAX (the "infinity" sentinel for "current").
    pub fn insert(&mut self, values: Vec<u64>) {
        let now = now_millis();
        self.last_modified = now;
        self.current_rows.push((values, now));
    }

    /// Update a row matching the predicate. The old row is moved to history
    /// with ValidTo = now; the new row is inserted with ValidFrom = now.
    pub fn update<F>(&mut self, predicate: F, new_values: Vec<u64>)
    where
        F: Fn(&[u64]) -> bool,
    {
        let now = now_millis();
        let mut i = 0;
        while i < self.current_rows.len() {
            if predicate(&self.current_rows[i].0) {
                let (old_values, valid_from) = &self.current_rows[i];
                // Move old row to history.
                self.history.push(HistoryRow {
                    values: old_values.clone(),
                    valid_from: *valid_from,
                    valid_to: now,
                });
                // Replace with new values.
                self.current_rows[i] = (new_values.clone(), now);
                self.last_modified = now;
                return;
            }
            i += 1;
        }
    }

    /// Delete rows matching the predicate. Deleted rows go to history.
    pub fn delete<F>(&mut self, predicate: F) -> usize
    where
        F: Fn(&[u64]) -> bool,
    {
        let now = now_millis();
        let mut deleted = 0;
        let mut i = 0;
        while i < self.current_rows.len() {
            if predicate(&self.current_rows[i].0) {
                let (values, valid_from) = &self.current_rows[i];
                self.history.push(HistoryRow {
                    values: values.clone(),
                    valid_from: *valid_from,
                    valid_to: now,
                });
                self.current_rows.remove(i);
                deleted += 1;
            } else {
                i += 1;
            }
        }
        self.last_modified = now;
        deleted
    }

    /// Query AS OF a point in time: returns rows that were current at
    /// that timestamp (from either current rows or history).
    pub fn query_as_of(&self, timestamp: u64) -> Vec<Vec<u64>> {
        let mut result = Vec::new();
        // Check current rows (ValidFrom <= timestamp, ValidTo = MAX).
        for (row, valid_from) in &self.current_rows {
            if *valid_from <= timestamp {
                result.push(row.clone());
            }
        }
        // Check history rows (ValidFrom <= timestamp < ValidTo).
        for h in &self.history {
            if h.valid_from <= timestamp && timestamp < h.valid_to {
                result.push(h.values.clone());
            }
        }
        result
    }

    /// Query BETWEEN two timestamps: returns all row versions that were
    /// current at any point in [start, end].
    pub fn query_between(&self, start: u64, end: u64) -> Vec<HistoryRow> {
        let mut result = Vec::new();
        for h in &self.history {
            if h.valid_from <= end && h.valid_to >= start {
                result.push(h.clone());
            }
        }
        result
    }

    /// Query ALL: returns every version of every row (current + history).
    pub fn query_all(&self) -> Vec<HistoryRow> {
        let mut result = Vec::new();
        for h in &self.history {
            result.push(h.clone());
        }
        // Current rows are also a "version" with ValidTo = MAX.
        for (row, valid_from) in &self.current_rows {
            result.push(HistoryRow {
                values: row.clone(),
                valid_from: *valid_from,
                valid_to: u64::MAX,
            });
        }
        result
    }

    /// Count versions per row (identified by the first column = primary key).
    pub fn version_counts(&self) -> Vec<(u64, usize)> {
        use std::collections::HashMap;
        let mut counts: HashMap<u64, usize> = HashMap::new();
        for h in &self.history {
            if let Some(&pk) = h.values.first() {
                *counts.entry(pk).or_insert(0) += 1;
            }
        }
        for (row, _) in &self.current_rows {
            if let Some(&pk) = row.first() {
                *counts.entry(pk).or_insert(0) += 1;
            }
        }
        counts.into_iter().collect()
    }
}

/// Get the current time as epoch milliseconds.
pub fn now_millis() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_millis() as u64).unwrap_or(0)
}

// -----------------------------------------------------------------------
// Tests
// -----------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    fn make_temporal() -> TemporalTable {
        TemporalTable::new(vec!["id".into(), "val".into()])
    }

    #[test]
    fn insert_and_query_current() {
        let mut t = make_temporal();
        t.insert(vec![1, 100]);
        t.insert(vec![2, 200]);
        let now = now_millis();
        // Wait a tiny bit to ensure timestamp is >= insert time.
        let rows = t.query_as_of(now + 1);
        assert_eq!(rows.len(), 2);
    }

    #[test]
    fn update_creates_history() {
        let mut t = make_temporal();
        t.insert(vec![1, 100]);
        let ts1 = now_millis();
        std::thread::sleep(std::time::Duration::from_millis(10));
        t.update(|r| r[0] == 1, vec![1, 999]);
        let ts2 = now_millis();

        // AS OF ts1: should see the old value (100).
        let rows = t.query_as_of(ts1);
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0][1], 100);

        // AS OF ts2: should see the new value (999).
        let rows = t.query_as_of(ts2 + 1);
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0][1], 999);
    }

    #[test]
    fn delete_creates_history() {
        let mut t = make_temporal();
        t.insert(vec![1, 100]);
        t.insert(vec![2, 200]);
        let ts1 = now_millis();
        std::thread::sleep(std::time::Duration::from_millis(10));
        let deleted = t.delete(|r| r[0] == 1);
        assert_eq!(deleted, 1);

        // Current should have 1 row (id=2).
        assert_eq!(t.current_rows.len(), 1); // this still works with Vec<(Vec<u64>, u64)>

        // AS OF ts1: should see both rows.
        let rows = t.query_as_of(ts1);
        assert_eq!(rows.len(), 2);
    }

    #[test]
    fn query_between() {
        let mut t = make_temporal();
        t.insert(vec![1, 100]);
        let ts1 = now_millis();
        std::thread::sleep(std::time::Duration::from_millis(10));
        t.update(|r| r[0] == 1, vec![1, 200]);
        let ts2 = now_millis();
        std::thread::sleep(std::time::Duration::from_millis(10));
        t.update(|r| r[0] == 1, vec![1, 300]);
        let ts3 = now_millis();

        // BETWEEN ts1 and ts3: should see 2 history versions.
        let versions = t.query_between(ts1, ts3);
        assert!(versions.len() >= 2);
    }

    #[test]
    fn query_all_versions() {
        let mut t = make_temporal();
        t.insert(vec![1, 100]);
        std::thread::sleep(std::time::Duration::from_millis(10));
        t.update(|r| r[0] == 1, vec![1, 200]);
        std::thread::sleep(std::time::Duration::from_millis(10));
        t.update(|r| r[0] == 1, vec![1, 300]);

        let all = t.query_all();
        // 2 history versions + 1 current = 3.
        assert_eq!(all.len(), 3);
    }

    #[test]
    fn version_counts() {
        let mut t = make_temporal();
        t.insert(vec![1, 100]);
        t.insert(vec![2, 200]);
        std::thread::sleep(std::time::Duration::from_millis(10));
        t.update(|r| r[0] == 1, vec![1, 150]);
        std::thread::sleep(std::time::Duration::from_millis(10));
        t.update(|r| r[0] == 1, vec![1, 175]);

        let counts = t.version_counts();
        // id=1 has 3 versions (2 history + 1 current). id=2 has 1.
        for (pk, count) in &counts {
            if *pk == 1 {
                assert_eq!(*count, 3);
            }
            if *pk == 2 {
                assert_eq!(*count, 1);
            }
        }
    }

    #[test]
    fn empty_table_query() {
        let t = make_temporal();
        assert_eq!(t.query_as_of(now_millis()).len(), 0);
        assert_eq!(t.query_all().len(), 0);
    }
}
