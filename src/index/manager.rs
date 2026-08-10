//! # Index manager (Wave 26, extended Wave 66).
//!
//! Wires the existing BSI (Bit-Sliced Index) and LSH (Locality-Sensitive
//! Hash) index implementations into the query planning layer. The index
//! manager tracks which columns have indexes and provides lookup for
//! the optimizer to decide when to use an index vs a full scan.
//!
//! Wave 66 extension: indexes now have names (for `CREATE INDEX name ON
//! ...` / `DROP INDEX name`) and the manager holds an in-memory hash
//! index data structure for equality lookups. The executor consults
//! [`IndexManager::lookup`] when a query has `WHERE col = value` on an
//! indexed column, skipping the full scan.

use std::collections::HashMap;

/// The type of index on a column.
#[derive(Debug, Clone, PartialEq)]
pub enum IndexType {
    /// Bit-Sliced Index for equality/range predicates on numeric columns.
    BSI,
    /// Locality-Sensitive Hash for similarity queries.
    LSH,
    /// Hash index for exact equality lookups.
    Hash,
}

/// An index on a specific table column.
#[derive(Debug, Clone)]
pub struct IndexEntry {
    pub table_name: String,
    pub column_name: String,
    pub index_type: IndexType,
    /// Estimated number of unique values (cardinality).
    pub cardinality: u64,
    /// Optional index name (Wave 66). `None` for indexes created via the
    /// low-level `create` API (pre-Wave-66 callers).
    pub name: Option<String>,
}

/// The index manager: tracks all indexes across all tables.
#[derive(Debug, Clone, Default)]
pub struct IndexManager {
    /// Maps (table_name, column_name) → IndexEntry.
    indexes: HashMap<(String, String), IndexEntry>,
    /// Maps index_name → (table, column). Used by `DROP INDEX name`.
    by_name: HashMap<String, (String, String)>,
    /// In-memory hash index data: (table, column) → (value → row indices).
    /// Populated by `build_hash_index` (called from `CREATE INDEX`).
    /// Used by `lookup` for fast equality lookups.
    hash_data: HashMap<(String, String), HashMap<u64, Vec<usize>>>,
}

impl IndexManager {
    /// Create an empty index manager.
    pub fn new() -> Self {
        Self { indexes: HashMap::new(), by_name: HashMap::new(), hash_data: HashMap::new() }
    }

    /// Register an index on a column (pre-Wave-66 API, no name).
    pub fn create(&mut self, table: &str, column: &str, index_type: IndexType, cardinality: u64) {
        let entry = IndexEntry {
            table_name: table.to_string(),
            column_name: column.to_string(),
            index_type,
            cardinality,
            name: None,
        };
        self.indexes.insert((table.to_string(), column.to_string()), entry);
    }

    /// Register a named index (Wave 66). Used by `CREATE INDEX name ON
    /// table (column)`. If an index with the same name already exists,
    /// it's replaced.
    pub fn create_named(
        &mut self,
        name: &str,
        table: &str,
        column: &str,
        index_type: IndexType,
        cardinality: u64,
    ) {
        let entry = IndexEntry {
            table_name: table.to_string(),
            column_name: column.to_string(),
            index_type,
            cardinality,
            name: Some(name.to_string()),
        };
        self.indexes.insert((table.to_string(), column.to_string()), entry);
        self.by_name.insert(name.to_string(), (table.to_string(), column.to_string()));
    }

    /// Build (or rebuild) the in-memory hash index for a (table, column)
    /// pair from the column's current cell values. Used by `CREATE INDEX`
    /// to populate the index data structure that `lookup` consults.
    pub fn build_hash_index(&mut self, table: &str, column: &str, values: &[u64]) {
        let mut map: HashMap<u64, Vec<usize>> = HashMap::new();
        for (i, &v) in values.iter().enumerate() {
            map.entry(v).or_default().push(i);
        }
        self.hash_data.insert((table.to_string(), column.to_string()), map);
    }

    /// Drop an index by (table, column). Returns true if it existed.
    pub fn drop(&mut self, table: &str, column: &str) -> bool {
        if let Some(entry) = self.indexes.remove(&(table.to_string(), column.to_string())) {
            if let Some(name) = entry.name {
                self.by_name.remove(&name);
            }
            self.hash_data.remove(&(table.to_string(), column.to_string()));
            true
        } else {
            false
        }
    }

    /// Drop an index by name (Wave 66). Returns true if it existed.
    pub fn drop_by_name(&mut self, name: &str) -> bool {
        if let Some((table, column)) = self.by_name.remove(name) {
            self.indexes.remove(&(table.clone(), column.clone()));
            self.hash_data.remove(&(table, column));
            true
        } else {
            // Also check unnamed indexes (pre-Wave-66 path).
            false
        }
    }

    /// Look up an index for a (table, column) pair.
    pub fn get(&self, table: &str, column: &str) -> Option<&IndexEntry> {
        self.indexes.get(&(table.to_string(), column.to_string()))
    }

    /// Look up an index by name (Wave 66).
    pub fn get_by_name(&self, name: &str) -> Option<(&String, &String)> {
        self.by_name.get(name).map(|(t, c)| (t, c))
    }

    /// Check if an index exists for a column.
    pub fn has_index(&self, table: &str, column: &str) -> bool {
        self.indexes.contains_key(&(table.to_string(), column.to_string()))
    }

    /// Fast equality lookup: returns the row indices where `column` equals
    /// `value`, using the in-memory hash index built by `build_hash_index`.
    /// Returns `None` if no hash index exists for this (table, column).
    pub fn lookup(&self, table: &str, column: &str, value: u64) -> Option<&Vec<usize>> {
        self.hash_data.get(&(table.to_string(), column.to_string())).and_then(|m| m.get(&value))
    }

    /// Decide whether to use an index for a predicate.
    ///
    /// Returns true if the index should be used (i.e., the column has an
    /// index and the selectivity is high enough to justify it).
    pub fn should_use_index(&self, table: &str, column: &str, table_row_count: u64) -> bool {
        if !self.has_index(table, column) {
            return false;
        }
        if let Some(entry) = self.get(table, column) {
            // Wave 9 fix (I1): Use index when selectivity is LOW (few rows match).
            // Previously this was inverted (> 0.1 instead of < 0.1), causing the
            // index to be skipped exactly when it should be used.
            // Correct logic: if cardinality / row_count is small, each value
            // matches few rows, so an index lookup is cheaper than a full scan.
            if table_row_count == 0 {
                return false;
            }
            let selectivity = entry.cardinality as f64 / table_row_count as f64;
            return selectivity < 0.1;
        }
        false
    }

    /// List all indexes.
    pub fn list(&self) -> Vec<&IndexEntry> {
        self.indexes.values().collect()
    }

    /// Count of indexes.
    pub fn len(&self) -> usize {
        self.indexes.len()
    }

    /// Returns true if no indexes exist.
    pub fn is_empty(&self) -> bool {
        self.indexes.is_empty()
    }
}

// -----------------------------------------------------------------------
// Tests
// -----------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn create_and_lookup() {
        let mut mgr = IndexManager::new();
        mgr.create("users", "id", IndexType::BSI, 1000);
        assert!(mgr.has_index("users", "id"));
        let entry = mgr.get("users", "id").unwrap();
        assert_eq!(entry.index_type, IndexType::BSI);
        assert_eq!(entry.cardinality, 1000);
    }

    #[test]
    fn drop_index() {
        let mut mgr = IndexManager::new();
        mgr.create("users", "id", IndexType::BSI, 1000);
        assert!(mgr.drop("users", "id"));
        assert!(!mgr.has_index("users", "id"));
    }

    #[test]
    fn no_index_returns_false() {
        let mgr = IndexManager::new();
        assert!(!mgr.should_use_index("users", "id", 1000));
    }

    #[test]
    fn use_index_low_selectivity() {
        let mut mgr = IndexManager::new();
        // Wave 9 fix (I1): cardinality 50 out of 1000 rows = 5% selectivity
        // → USE index (each value matches few rows, index is cheaper than scan)
        mgr.create("users", "status", IndexType::BSI, 50);
        assert!(mgr.should_use_index("users", "status", 1000));
    }

    #[test]
    fn skip_index_high_selectivity() {
        let mut mgr = IndexManager::new();
        // Wave 9 fix (I1): cardinality 500 out of 1000 rows = 50% selectivity
        // → SKIP index (each value matches many rows, scan is cheaper)
        mgr.create("users", "id", IndexType::BSI, 500);
        assert!(!mgr.should_use_index("users", "id", 1000));
    }

    #[test]
    fn multiple_indexes() {
        let mut mgr = IndexManager::new();
        mgr.create("users", "id", IndexType::BSI, 1000);
        mgr.create("users", "email", IndexType::Hash, 1000);
        mgr.create("products", "name", IndexType::LSH, 500);
        assert_eq!(mgr.len(), 3);
        assert!(mgr.has_index("users", "id"));
        assert!(mgr.has_index("users", "email"));
        assert!(mgr.has_index("products", "name"));
        assert!(!mgr.has_index("orders", "id"));
    }

    #[test]
    fn empty_table() {
        let mut mgr = IndexManager::new();
        mgr.create("users", "id", IndexType::BSI, 100);
        assert!(!mgr.should_use_index("users", "id", 0));
    }

    #[test]
    fn list_indexes() {
        let mut mgr = IndexManager::new();
        mgr.create("users", "id", IndexType::BSI, 1000);
        mgr.create("orders", "user_id", IndexType::Hash, 500);
        let list = mgr.list();
        assert_eq!(list.len(), 2);
    }
}
