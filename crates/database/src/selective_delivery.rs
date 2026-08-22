//! Coarse table-level selective-delivery hints for the current NATS shadow
//! path.
//!
//! This TTL registry is not a reactive-invalidation correctness authority: an
//! expired or delayed entry may omit a node. Runtime fanout must therefore not
//! rely on it until the replay-safe owner registration protocol in
//! `delta_interest` is durably integrated end to end. Runtime cutover also
//! requires durable owner state/delivery, owner fencing, a multi-owner
//! activation barrier, and catalog-version transition handling.

use std::{
    collections::{
        BTreeMap,
        BTreeSet,
    },
    sync::Arc,
    time::{
        Duration,
        SystemTime,
        UNIX_EPOCH,
    },
};

use anyhow::Context;
use async_nats::jetstream::{
    self,
    kv::{
        Operation,
        Store,
    },
};
use futures::StreamExt;
use parking_lot::RwLock;
use serde::{
    Deserialize,
    Serialize,
};
use value::TableName;

pub const SELECTIVE_DELIVERY_KV_BUCKET: &str = "convex_delta_interest";
pub const NODE_SUBJECT_PREFIX: &str = "convex.commits.node";
const INTEREST_MAX_AGE: Duration = Duration::from_secs(90);

#[derive(Clone)]
pub struct SelectiveDeliveryRegistry {
    node_name: String,
    kv: Store,
    cache: Arc<RwLock<BTreeMap<String, InterestRegistration>>>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
struct InterestRegistration {
    tables: Vec<String>,
    updated_at_ms: u64,
}

impl SelectiveDeliveryRegistry {
    pub async fn connect(nats_url: &str, node_name: String) -> anyhow::Result<Self> {
        let _ = rustls::crypto::ring::default_provider().install_default();
        let client = async_nats::connect(nats_url)
            .await
            .with_context(|| format!("Failed to connect to NATS at {nats_url}"))?;
        Self::from_client(client, node_name).await
    }

    pub async fn from_client(
        client: async_nats::Client,
        node_name: String,
    ) -> anyhow::Result<Self> {
        let jetstream = jetstream::new(client);
        let kv = jetstream
            .create_key_value(async_nats::jetstream::kv::Config {
                bucket: SELECTIVE_DELIVERY_KV_BUCKET.to_string(),
                history: 1,
                max_age: INTEREST_MAX_AGE.saturating_mul(4),
                ..Default::default()
            })
            .await
            .context("Failed to create selective-delivery KV bucket")?;

        let cache = Arc::new(RwLock::new(BTreeMap::new()));
        Self::prime_cache(&kv, &cache).await?;
        Self::spawn_watcher(kv.clone(), cache.clone());

        Ok(Self {
            node_name,
            kv,
            cache,
        })
    }

    pub async fn publish_local_interest(&self, tables: &BTreeSet<TableName>) -> anyhow::Result<()> {
        let registration = InterestRegistration {
            tables: tables.iter().map(ToString::to_string).collect(),
            updated_at_ms: now_ms(),
        };
        let payload = serde_json::to_vec(&registration)
            .context("Failed to serialize selective-delivery interest registration")?;
        self.kv
            .put(&self.node_name, payload.into())
            .await
            .with_context(|| {
                format!(
                    "Failed to publish selective-delivery interest for {}",
                    self.node_name
                )
            })?;
        self.cache
            .write()
            .insert(self.node_name.clone(), registration);
        Ok(())
    }

    pub fn interested_nodes_for_tables(&self, tables: &BTreeSet<TableName>) -> Vec<String> {
        interested_nodes_for_tables_from_cache(&self.cache.read(), now_ms(), tables)
    }

    pub fn node_subject(node_name: &str) -> String {
        format!("{NODE_SUBJECT_PREFIX}.{node_name}")
    }

    async fn prime_cache(
        kv: &Store,
        cache: &Arc<RwLock<BTreeMap<String, InterestRegistration>>>,
    ) -> anyhow::Result<()> {
        let keys = kv
            .keys()
            .await
            .context("Failed to list selective-delivery keys")?;
        let keys: Vec<_> = keys
            .filter_map(|result| async move { result.ok() })
            .collect()
            .await;
        for key in keys {
            let Some(entry) = kv
                .entry(&key)
                .await
                .with_context(|| format!("Failed to read selective-delivery entry {key}"))?
            else {
                continue;
            };
            if let Ok(registration) = serde_json::from_slice::<InterestRegistration>(&entry.value) {
                cache.write().insert(key, registration);
            }
        }
        Ok(())
    }

    fn spawn_watcher(kv: Store, cache: Arc<RwLock<BTreeMap<String, InterestRegistration>>>) {
        tokio::spawn(async move {
            let mut watch = match kv.watch_all().await {
                Ok(watch) => watch,
                Err(e) => {
                    tracing::warn!("Selective-delivery registry watch failed to start: {e:#}");
                    return;
                },
            };
            while let Some(result) = watch.next().await {
                match result {
                    Ok(entry) => match entry.operation {
                        Operation::Put => {
                            match serde_json::from_slice::<InterestRegistration>(&entry.value) {
                                Ok(registration) => {
                                    cache.write().insert(entry.key, registration);
                                },
                                Err(e) => {
                                    tracing::warn!(
                                        "Failed to decode selective-delivery interest entry {}: \
                                         {e:#}",
                                        entry.key
                                    );
                                },
                            }
                        },
                        Operation::Delete | Operation::Purge => {
                            cache.write().remove(&entry.key);
                        },
                    },
                    Err(e) => {
                        tracing::warn!("Selective-delivery registry watch error: {e:#}");
                    },
                }
            }
        });
    }
}

fn interested_nodes_for_tables_from_cache(
    cache: &BTreeMap<String, InterestRegistration>,
    now_ms: u64,
    tables: &BTreeSet<TableName>,
) -> Vec<String> {
    if tables.is_empty() {
        return Vec::new();
    }
    cache
        .iter()
        .filter(|(_, registration)| registration.is_fresh(now_ms))
        .filter(|(_, registration)| registration.matches(tables))
        .map(|(node, _)| node.clone())
        .collect()
}

impl InterestRegistration {
    fn is_fresh(&self, now_ms: u64) -> bool {
        now_ms.saturating_sub(self.updated_at_ms) <= INTEREST_MAX_AGE.as_millis() as u64
    }

    fn matches(&self, tables: &BTreeSet<TableName>) -> bool {
        self.tables.iter().any(|table| {
            table
                .parse::<TableName>()
                .ok()
                .is_some_and(|table_name| tables.contains(&table_name))
        })
    }
}

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64
}

#[cfg(test)]
mod tests {
    use std::collections::{
        BTreeMap,
        BTreeSet,
    };

    use value::TableName;

    use crate::selective_delivery::{
        interested_nodes_for_tables_from_cache,
        InterestRegistration,
        SelectiveDeliveryRegistry,
    };

    #[test]
    fn interested_nodes_match_tables_and_skip_stale_entries() {
        let now_ms = 1_000_000;
        let messages: TableName = "messages".parse().unwrap();
        let tasks: TableName = "tasks".parse().unwrap();
        let cache = BTreeMap::from([
            (
                "node-a".to_string(),
                InterestRegistration {
                    tables: vec!["messages".to_string()],
                    updated_at_ms: now_ms,
                },
            ),
            (
                "node-b".to_string(),
                InterestRegistration {
                    tables: vec!["tasks".to_string()],
                    updated_at_ms: now_ms.saturating_sub(200_000),
                },
            ),
        ]);
        assert_eq!(
            interested_nodes_for_tables_from_cache(&cache, now_ms, &BTreeSet::from([messages])),
            vec!["node-a".to_string()]
        );
        assert!(
            interested_nodes_for_tables_from_cache(&cache, now_ms, &BTreeSet::from([tasks]))
                .is_empty()
        );
    }

    #[test]
    fn node_subject_names_are_stable() {
        assert_eq!(
            SelectiveDeliveryRegistry::node_subject("node-a"),
            "convex.commits.node.node-a"
        );
    }
}
