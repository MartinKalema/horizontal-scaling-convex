//! Partition assignment for table-level write scaling.
//!
//! Maps tables to partitions (nodes). Each node owns a set of tables
//! and is the single Committer for those tables, preserving the
//! single-writer guarantee within each partition.
//!
//! ## Design (inspired by Vitess VSchema)
//!
//! A `PartitionMap` assigns each table name to a `PartitionId`.
//! The partition map is loaded from configuration at startup and
//! shared across all nodes. All nodes must agree on the mapping.
//!
//! Tables not explicitly assigned go to partition 0 (the default
//! partition). System tables (`_tables`, `_index`, `_modules`, etc.)
//! are always on partition 0.
//!
//! ## Usage
//!
//! ```ignore
//! let map = PartitionMap::from_config("messages=1,users=1,projects=2,tasks=2");
//! assert_eq!(map.partition_for_table(&"messages".parse().unwrap()), PartitionId(1));
//! assert_eq!(map.partition_for_table(&"_tables".parse().unwrap()), PartitionId(0));
//! ```

use std::{
    collections::BTreeMap,
    fmt,
};

use value::TableName;

/// Identifies a partition (node) in the cluster.
/// Partition 0 is the default and owns all system tables.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct PartitionId(pub u32);

impl PartitionId {
    /// The default partition that owns system tables and any unassigned
    /// user tables.
    pub const DEFAULT: Self = Self(0);
}

impl fmt::Display for PartitionId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "partition-{}", self.0)
    }
}

/// Maps table names to partitions.
///
/// System tables (starting with `_`) are always on partition 0.
/// User tables are assigned based on the configuration.
/// Unassigned user tables default to partition 0.
#[derive(Clone, Debug)]
pub struct PartitionMap {
    /// Explicit table → partition assignments.
    assignments: BTreeMap<TableName, PartitionId>,
    /// The partition this node owns.
    local_partition: PartitionId,
    /// Total number of partitions in the cluster.
    num_partitions: u32,
}

impl PartitionMap {
    /// Create a partition map from a config string.
    ///
    /// Format: `"table1=1,table2=1,table3=2,table4=2"`
    ///
    /// Tables not listed default to partition 0.
    /// System tables (starting with `_`) are always partition 0 regardless.
    pub fn from_config(config: &str, local_partition: PartitionId, num_partitions: u32) -> Self {
        let mut assignments = BTreeMap::new();
        if !config.is_empty() {
            for pair in config.split(',') {
                let pair = pair.trim();
                if let Some((table, partition)) = pair.split_once('=') {
                    let table = table.trim();
                    let partition = partition.trim();
                    if let (Ok(table_name), Ok(partition_id)) =
                        (table.parse::<TableName>(), partition.parse::<u32>())
                    {
                        assignments.insert(table_name, PartitionId(partition_id));
                    }
                }
            }
        }
        Self {
            assignments,
            local_partition,
            num_partitions,
        }
    }

    /// Create a single-partition map (all tables on partition 0).
    /// Used for single-node deployments and backward compatibility.
    pub fn single_partition() -> Self {
        Self {
            assignments: BTreeMap::new(),
            local_partition: PartitionId::DEFAULT,
            num_partitions: 1,
        }
    }

    /// Get the partition that owns the given table.
    ///
    /// System tables (starting with `_`) always return partition 0.
    /// User tables return their configured partition, or partition 0 if
    /// not explicitly assigned.
    pub fn partition_for_table(&self, table: &TableName) -> PartitionId {
        // System tables always on partition 0.
        if table.is_system() {
            return PartitionId::DEFAULT;
        }
        self.assignments
            .get(table)
            .copied()
            .unwrap_or(PartitionId::DEFAULT)
    }

    /// Check if this node owns the given table.
    pub fn is_local(&self, table: &TableName) -> bool {
        self.partition_for_table(table) == self.local_partition
    }

    /// Get all tables assigned to a specific partition.
    pub fn tables_for_partition(&self, partition: PartitionId) -> Vec<TableName> {
        self.assignments
            .iter()
            .filter(|(_, p)| **p == partition)
            .map(|(t, _)| t.clone())
            .collect()
    }

    /// Get this node's partition ID.
    pub fn local_partition(&self) -> PartitionId {
        self.local_partition
    }

    /// Get the total number of partitions.
    pub fn num_partitions(&self) -> u32 {
        self.num_partitions
    }

    /// Get all partition IDs in the cluster.
    pub fn all_partitions(&self) -> Vec<PartitionId> {
        (0..self.num_partitions).map(PartitionId).collect()
    }

    /// Get all explicit assignments.
    pub fn assignments(&self) -> &BTreeMap<TableName, PartitionId> {
        &self.assignments
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_single_partition() {
        let map = PartitionMap::single_partition();
        assert_eq!(
            map.partition_for_table(&"messages".parse().unwrap()),
            PartitionId::DEFAULT
        );
        assert_eq!(
            map.partition_for_table(&"_tables".parse().unwrap()),
            PartitionId::DEFAULT
        );
        assert!(map.is_local(&"anything".parse().unwrap()));
    }

    #[test]
    fn test_multi_partition() {
        let map =
            PartitionMap::from_config("messages=1,users=1,projects=2,tasks=2", PartitionId(1), 3);

        // Assigned tables.
        assert_eq!(
            map.partition_for_table(&"messages".parse().unwrap()),
            PartitionId(1)
        );
        assert_eq!(
            map.partition_for_table(&"users".parse().unwrap()),
            PartitionId(1)
        );
        assert_eq!(
            map.partition_for_table(&"projects".parse().unwrap()),
            PartitionId(2)
        );
        assert_eq!(
            map.partition_for_table(&"tasks".parse().unwrap()),
            PartitionId(2)
        );

        // Unassigned user table defaults to partition 0.
        assert_eq!(
            map.partition_for_table(&"other".parse().unwrap()),
            PartitionId::DEFAULT
        );

        // System tables always partition 0.
        assert_eq!(
            map.partition_for_table(&"_tables".parse().unwrap()),
            PartitionId::DEFAULT
        );
        assert_eq!(
            map.partition_for_table(&"_modules".parse().unwrap()),
            PartitionId::DEFAULT
        );

        // Local ownership.
        assert!(map.is_local(&"messages".parse().unwrap()));
        assert!(map.is_local(&"users".parse().unwrap()));
        assert!(!map.is_local(&"projects".parse().unwrap()));
        assert!(!map.is_local(&"tasks".parse().unwrap()));
        // System tables are on partition 0, not partition 1.
        assert!(!map.is_local(&"_tables".parse().unwrap()));

        // Tables for partition.
        let p1_tables = map.tables_for_partition(PartitionId(1));
        assert_eq!(p1_tables.len(), 2);
        let p2_tables = map.tables_for_partition(PartitionId(2));
        assert_eq!(p2_tables.len(), 2);
    }

    #[test]
    fn test_empty_config() {
        let map = PartitionMap::from_config("", PartitionId(0), 1);
        assert_eq!(
            map.partition_for_table(&"messages".parse().unwrap()),
            PartitionId::DEFAULT
        );
    }

    #[test]
    fn test_all_partitions() {
        let map = PartitionMap::from_config("a=1,b=2", PartitionId(0), 3);
        let all = map.all_partitions();
        assert_eq!(all, vec![PartitionId(0), PartitionId(1), PartitionId(2)]);
    }
}
