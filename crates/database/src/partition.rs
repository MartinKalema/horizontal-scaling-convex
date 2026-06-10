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
    sync::{
        Arc,
        RwLock,
    },
};

use value::TableName;

use crate::write_log::WriteSource;

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

/// Monotonically identifies the placement metadata version used for routing.
///
/// Version 0 is the static, startup-configured map. Dynamic placement updates
/// must bump this value whenever ownership changes.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct PlacementVersion(pub u64);

impl PlacementVersion {
    pub const STATIC: Self = Self(0);

    pub fn new(version: u64) -> Self {
        Self(version)
    }
}

impl From<PlacementVersion> for u64 {
    fn from(version: PlacementVersion) -> Self {
        version.0
    }
}

impl From<u64> for PlacementVersion {
    fn from(version: u64) -> Self {
        Self(version)
    }
}

impl fmt::Display for PlacementVersion {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "placement-version-{}", self.0)
    }
}

/// Where a placement map was loaded from.
///
/// The static config source preserves today's env-var path. The replicated
/// source is the #130 target: placement metadata owned by the cluster rather
/// than by each process's startup environment.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PlacementMetadataSource {
    StaticConfig,
    Replicated,
}

/// Logical object owned by a partition.
///
/// We only route whole tables today. Keeping the target explicit prevents the
/// control-plane model from baking in table-only ownership when #130 evolves
/// toward range or key-prefix placement.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum PlacementTarget {
    Table(TableName),
}

/// One ownership rule in the placement metadata.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PlacementRule {
    pub target: PlacementTarget,
    pub owner: PartitionId,
}

/// Static placement inputs from process config.
pub struct StaticPlacementConfig<'a> {
    pub table_assignments: &'a str,
    pub num_partitions: u32,
    pub placement_version: PlacementVersion,
}

/// Versioned placement metadata used to build runtime routing state.
///
/// This is the seam between the future replicated control-plane record and the
/// existing `PartitionMap` used by commit, routing, and 2PC paths.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PlacementMetadata {
    source: PlacementMetadataSource,
    version: PlacementVersion,
    num_partitions: u32,
    rules: BTreeMap<PlacementTarget, PartitionId>,
}

impl PlacementMetadata {
    pub fn from_static_config(config: StaticPlacementConfig<'_>) -> Self {
        let mut rules = BTreeMap::new();
        if !config.table_assignments.is_empty() {
            for pair in config.table_assignments.split(',') {
                let pair = pair.trim();
                if let Some((table, partition)) = pair.split_once('=') {
                    let table = table.trim();
                    let partition = partition.trim();
                    if let (Ok(table_name), Ok(partition_id)) =
                        (table.parse::<TableName>(), partition.parse::<u32>())
                    {
                        rules.insert(
                            PlacementTarget::Table(table_name),
                            PartitionId(partition_id),
                        );
                    }
                }
            }
        }
        Self {
            source: PlacementMetadataSource::StaticConfig,
            version: config.placement_version,
            num_partitions: config.num_partitions,
            rules,
        }
    }

    pub fn from_partition_map(
        partition_map: &PartitionMap,
        source: PlacementMetadataSource,
    ) -> Self {
        Self {
            source,
            version: partition_map.placement_version(),
            num_partitions: partition_map.num_partitions(),
            rules: partition_map
                .assignments()
                .iter()
                .map(|(table, owner)| (PlacementTarget::Table(table.clone()), *owner))
                .collect(),
        }
    }

    pub fn source(&self) -> PlacementMetadataSource {
        self.source
    }

    pub fn version(&self) -> PlacementVersion {
        self.version
    }

    pub fn num_partitions(&self) -> u32 {
        self.num_partitions
    }

    pub fn rules(&self) -> Vec<PlacementRule> {
        self.rules
            .iter()
            .map(|(target, owner)| PlacementRule {
                target: target.clone(),
                owner: *owner,
            })
            .collect()
    }

    pub fn table_assignments(&self) -> BTreeMap<TableName, PartitionId> {
        self.rules
            .iter()
            .filter_map(|(target, owner)| match target {
                PlacementTarget::Table(table) => Some((table.clone(), *owner)),
            })
            .collect()
    }

    pub fn into_partition_map(&self, local_partition: PartitionId) -> PartitionMap {
        PartitionMap {
            assignments: self.table_assignments(),
            local_partition,
            num_partitions: self.num_partitions,
            placement_version: self.version,
        }
    }
}

/// Refreshable placement state shared by routing components.
///
/// Callers take a `PartitionMap` snapshot before classifying or coordinating a
/// transaction. That keeps a single transaction internally consistent even if a
/// newer placement version is installed concurrently.
#[derive(Clone, Debug)]
pub struct PlacementState {
    local_partition: PartitionId,
    metadata: Arc<RwLock<PlacementMetadata>>,
}

impl PlacementState {
    pub fn new(local_partition: PartitionId, metadata: PlacementMetadata) -> anyhow::Result<Self> {
        Self::validate_local_partition(local_partition, metadata.num_partitions())?;
        Ok(Self {
            local_partition,
            metadata: Arc::new(RwLock::new(metadata)),
        })
    }

    pub fn from_partition_map(partition_map: PartitionMap) -> Self {
        let metadata = PlacementMetadata::from_partition_map(
            &partition_map,
            PlacementMetadataSource::StaticConfig,
        );
        Self {
            local_partition: partition_map.local_partition(),
            metadata: Arc::new(RwLock::new(metadata)),
        }
    }

    pub fn partition_map(&self) -> PartitionMap {
        self.metadata
            .read()
            .expect("placement metadata lock poisoned")
            .into_partition_map(self.local_partition)
    }

    pub fn refresh(&self, metadata: PlacementMetadata) -> anyhow::Result<()> {
        Self::validate_local_partition(self.local_partition, metadata.num_partitions())?;
        let mut current = self
            .metadata
            .write()
            .expect("placement metadata lock poisoned");
        anyhow::ensure!(
            metadata.version() >= current.version(),
            "Refusing to refresh placement metadata from {} down to {}",
            current.version(),
            metadata.version(),
        );
        *current = metadata;
        Ok(())
    }

    pub fn placement_version(&self) -> PlacementVersion {
        self.metadata
            .read()
            .expect("placement metadata lock poisoned")
            .version()
    }

    pub fn local_partition(&self) -> PartitionId {
        self.local_partition
    }

    pub fn num_partitions(&self) -> u32 {
        self.metadata
            .read()
            .expect("placement metadata lock poisoned")
            .num_partitions()
    }

    fn validate_local_partition(
        local_partition: PartitionId,
        num_partitions: u32,
    ) -> anyhow::Result<()> {
        anyhow::ensure!(
            local_partition.0 < num_partitions,
            "Local partition {} is outside placement metadata with {} partitions",
            local_partition,
            num_partitions,
        );
        Ok(())
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
    /// Version of the placement metadata used to build this map.
    placement_version: PlacementVersion,
}

impl PartitionMap {
    /// Create a partition map from a config string.
    ///
    /// Format: `"table1=1,table2=1,table3=2,table4=2"`
    ///
    /// Tables not listed default to partition 0.
    /// System tables (starting with `_`) are always partition 0 regardless.
    pub fn from_config(config: &str, local_partition: PartitionId, num_partitions: u32) -> Self {
        Self::from_config_with_version(
            config,
            local_partition,
            num_partitions,
            PlacementVersion::STATIC,
        )
    }

    /// Create a partition map from a config string and placement version.
    pub fn from_config_with_version(
        config: &str,
        local_partition: PartitionId,
        num_partitions: u32,
        placement_version: PlacementVersion,
    ) -> Self {
        PlacementMetadata::from_static_config(StaticPlacementConfig {
            table_assignments: config,
            num_partitions,
            placement_version,
        })
        .into_partition_map(local_partition)
    }

    /// Create a single-partition map (all tables on partition 0).
    /// Used for single-node deployments and backward compatibility.
    pub fn single_partition() -> Self {
        Self {
            assignments: BTreeMap::new(),
            local_partition: PartitionId::DEFAULT,
            num_partitions: 1,
            placement_version: PlacementVersion::STATIC,
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

    /// Get the placement metadata version.
    pub fn placement_version(&self) -> PlacementVersion {
        self.placement_version
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

pub fn uses_metadata_owner(write_source: &WriteSource) -> bool {
    matches!(write_source.as_str(), Some("start_push" | "finish_push"))
}

/// Returns the authoritative partition for tables that should be coordinated
/// cluster-wide for the current operation.
///
/// - User tables always have a single partition owner.
/// - Deploy metadata operations (`start_push`, `finish_push`) route system
///   tables to the metadata owner on partition 0 (TiDB DDL owner pattern).
/// - Other system-table writes are node-local operational state and therefore
///   return `None`.
pub fn routed_partition_for_table(
    table: &TableName,
    partition_map: &PartitionMap,
    write_source: &WriteSource,
) -> Option<PartitionId> {
    if !table.is_system() || uses_metadata_owner(write_source) {
        Some(partition_map.partition_for_table(table))
    } else {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::write_log::WriteSource;

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

    #[test]
    fn test_placement_version_defaults_to_static() {
        let map = PartitionMap::from_config("a=1", PartitionId(0), 2);
        assert_eq!(map.placement_version(), PlacementVersion::STATIC);
    }

    #[test]
    fn test_placement_version_from_config() {
        let map = PartitionMap::from_config_with_version(
            "a=1",
            PartitionId(0),
            2,
            PlacementVersion::new(42),
        );
        assert_eq!(map.placement_version(), PlacementVersion::new(42));
    }

    #[test]
    fn test_static_placement_metadata_builds_runtime_partition_map() {
        let metadata = PlacementMetadata::from_static_config(StaticPlacementConfig {
            table_assignments: "messages=0,projects=1",
            num_partitions: 2,
            placement_version: PlacementVersion::new(7),
        });

        assert_eq!(metadata.source(), PlacementMetadataSource::StaticConfig);
        assert_eq!(metadata.version(), PlacementVersion::new(7));
        assert_eq!(metadata.num_partitions(), 2);
        assert_eq!(
            metadata
                .table_assignments()
                .get(&"projects".parse().unwrap()),
            Some(&PartitionId(1)),
        );

        let map = metadata.into_partition_map(PartitionId(1));
        assert_eq!(
            map.partition_for_table(&"messages".parse().unwrap()),
            PartitionId(0),
        );
        assert_eq!(
            map.partition_for_table(&"projects".parse().unwrap()),
            PartitionId(1),
        );
        assert!(map.is_local(&"projects".parse().unwrap()));
        assert_eq!(map.placement_version(), PlacementVersion::new(7));
    }

    #[test]
    fn test_static_placement_metadata_keeps_table_targets_explicit() {
        let metadata = PlacementMetadata::from_static_config(StaticPlacementConfig {
            table_assignments: "messages=0,projects=1",
            num_partitions: 2,
            placement_version: PlacementVersion::new(3),
        });
        let rules = metadata.rules();

        assert!(rules.contains(&PlacementRule {
            target: PlacementTarget::Table("messages".parse().unwrap()),
            owner: PartitionId(0),
        }));
        assert!(rules.contains(&PlacementRule {
            target: PlacementTarget::Table("projects".parse().unwrap()),
            owner: PartitionId(1),
        }));
    }

    #[test]
    fn test_placement_state_refreshes_to_newer_metadata() -> anyhow::Result<()> {
        let state = PlacementState::new(
            PartitionId(1),
            PlacementMetadata::from_static_config(StaticPlacementConfig {
                table_assignments: "messages=0,projects=1",
                num_partitions: 2,
                placement_version: PlacementVersion::new(1),
            }),
        )?;

        state.refresh(PlacementMetadata::from_static_config(
            StaticPlacementConfig {
                table_assignments: "messages=1,projects=0",
                num_partitions: 2,
                placement_version: PlacementVersion::new(2),
            },
        ))?;

        let map = state.partition_map();
        assert_eq!(state.placement_version(), PlacementVersion::new(2));
        assert_eq!(
            map.partition_for_table(&"messages".parse().unwrap()),
            PartitionId(1),
        );
        assert_eq!(
            map.partition_for_table(&"projects".parse().unwrap()),
            PartitionId(0),
        );
        Ok(())
    }

    #[test]
    fn test_placement_state_rejects_version_downgrade() -> anyhow::Result<()> {
        let state = PlacementState::new(
            PartitionId(0),
            PlacementMetadata::from_static_config(StaticPlacementConfig {
                table_assignments: "messages=0",
                num_partitions: 2,
                placement_version: PlacementVersion::new(3),
            }),
        )?;

        let err = state
            .refresh(PlacementMetadata::from_static_config(
                StaticPlacementConfig {
                    table_assignments: "messages=1",
                    num_partitions: 2,
                    placement_version: PlacementVersion::new(2),
                },
            ))
            .unwrap_err();

        assert!(err
            .to_string()
            .contains("Refusing to refresh placement metadata"));
        assert_eq!(state.placement_version(), PlacementVersion::new(3));
        Ok(())
    }

    #[test]
    fn test_routed_partition_for_table_keeps_non_deploy_system_tables_local() {
        let map = PartitionMap::from_config("messages=1", PartitionId(1), 2);
        assert_eq!(
            routed_partition_for_table(
                &"_modules".parse().unwrap(),
                &map,
                &WriteSource::new("cron_tick"),
            ),
            None
        );
    }

    #[test]
    fn test_routed_partition_for_table_routes_deploy_system_tables_to_owner() {
        let map = PartitionMap::from_config("messages=1", PartitionId(1), 2);
        assert_eq!(
            routed_partition_for_table(
                &"_modules".parse().unwrap(),
                &map,
                &WriteSource::new("finish_push"),
            ),
            Some(PartitionId::DEFAULT)
        );
    }
}
