//! Global Timestamp Oracle (TSO) for distributed timestamp assignment.
//!
//! In a partitioned-write architecture, multiple Committers run on different
//! nodes. Each needs globally unique, monotonically increasing timestamps.
//! The TSO ensures no two nodes assign the same timestamp.
//!
//! ## Design (inspired by Vitess sequence tables)
//!
//! A central counter is stored in NATS KV. Each node reserves a batch of
//! timestamps (e.g., 1000 at a time). Within the batch, timestamps are
//! assigned locally with zero network calls. When the batch is exhausted,
//! the node reserves another batch from the central counter.
//!
//! This gives the performance of local timestamp assignment with the
//! correctness of global ordering.
//!
//! ## Implementations
//!
//! - [`LocalTimestampOracle`]: Wraps the existing Committer logic for
//!   single-node deployments. No behavior change.
//! - [`BatchTimestampOracle`]: Reserves batches from NATS KV for
//!   multi-node deployments.

use std::sync::Arc;

use anyhow::Context;
use async_trait::async_trait;
use common::{
    runtime::Runtime,
    types::Timestamp,
};
use parking_lot::Mutex;

/// Trait for assigning globally unique, monotonically increasing timestamps.
///
/// Each Committer calls `next_ts()` before committing a transaction.
/// The implementation must guarantee that no two calls — across any node
/// in the cluster — ever return the same timestamp.
#[async_trait]
pub trait TimestampOracle: Send + Sync + 'static {
    /// Get the next globally unique timestamp.
    /// Must be monotonically increasing within each node.
    /// Must not overlap with timestamps from other nodes.
    async fn next_ts(&self) -> anyhow::Result<Timestamp>;

    /// Get the current maximum committed timestamp across all nodes.
    /// Used for read-after-write consistency.
    async fn max_committed_ts(&self) -> anyhow::Result<Timestamp>;

    /// Advance the max committed timestamp. Called after a successful commit.
    async fn advance_committed_ts(&self, ts: Timestamp) -> anyhow::Result<()>;
}

/// Local timestamp oracle for single-node deployments.
/// Delegates to the system clock + monotonic counter, matching the existing
/// Committer behavior. No network calls, no external dependencies.
pub struct LocalTimestampOracle<RT: Runtime> {
    runtime: RT,
    state: Mutex<LocalState>,
}

struct LocalState {
    last_assigned: Timestamp,
    max_committed: Timestamp,
}

impl<RT: Runtime> LocalTimestampOracle<RT> {
    pub fn new(runtime: RT) -> Self {
        Self {
            runtime,
            state: Mutex::new(LocalState {
                last_assigned: Timestamp::MIN,
                max_committed: Timestamp::MIN,
            }),
        }
    }
}

#[async_trait]
impl<RT: Runtime> TimestampOracle for LocalTimestampOracle<RT> {
    async fn next_ts(&self) -> anyhow::Result<Timestamp> {
        let mut state = self.state.lock();
        let system_ts = self.runtime.generate_timestamp()?;
        let next = std::cmp::max(system_ts, state.last_assigned.succ()?);
        state.last_assigned = next;
        Ok(next)
    }

    async fn max_committed_ts(&self) -> anyhow::Result<Timestamp> {
        Ok(self.state.lock().max_committed)
    }

    async fn advance_committed_ts(&self, ts: Timestamp) -> anyhow::Result<()> {
        let mut state = self.state.lock();
        if ts > state.max_committed {
            state.max_committed = ts;
        }
        Ok(())
    }
}

/// Batch timestamp oracle for multi-node deployments.
///
/// Reserves ranges of timestamps from a central NATS KV counter.
/// Within a range, timestamps are assigned locally with zero network calls.
/// When the range is exhausted, a new range is reserved.
///
/// Example with batch_size=1000:
/// - Node A reserves [1000, 1999]
/// - Node B reserves [2000, 2999]
/// - Node A assigns 1000, 1001, 1002... locally
/// - Node B assigns 2000, 2001, 2002... locally
/// - No overlap, no coordination within a batch
pub struct BatchTimestampOracle {
    nats_client: async_nats::Client,
    kv_bucket: String,
    batch_size: u64,
    state: Mutex<BatchState>,
}

struct BatchState {
    /// Current position within the reserved range.
    current: u64,
    /// Upper bound (exclusive) of the reserved range.
    upper_bound: u64,
    /// Maximum committed timestamp seen.
    max_committed: Timestamp,
}

const TSO_COUNTER_KEY: &str = "tso_counter";
const TSO_MAX_COMMITTED_KEY: &str = "tso_max_committed";
const DEFAULT_BATCH_SIZE: u64 = 1000;

impl BatchTimestampOracle {
    /// Connect to NATS and initialize the KV bucket for timestamp allocation.
    pub async fn connect(
        nats_url: &str,
        batch_size: Option<u64>,
    ) -> anyhow::Result<Self> {
        // Reuse the crypto provider if already installed.
        let _ = rustls::crypto::ring::default_provider().install_default();

        let client = async_nats::connect(nats_url)
            .await
            .with_context(|| format!("TSO: Failed to connect to NATS at {nats_url}"))?;

        let jetstream = async_nats::jetstream::new(client.clone());

        // Create or get the KV bucket for TSO state.
        let kv = jetstream
            .create_key_value(async_nats::jetstream::kv::Config {
                bucket: "convex_tso".to_string(),
                history: 1,
                ..Default::default()
            })
            .await
            .context("TSO: Failed to create KV bucket")?;

        // Initialize counter if it doesn't exist.
        let initial_ts = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos() as u64;

        // Try to create the key. If it already exists, that's fine.
        match kv.create(TSO_COUNTER_KEY, initial_ts.to_be_bytes().to_vec().into()).await {
            Ok(_) => tracing::info!("TSO: Initialized counter at {initial_ts}"),
            Err(_) => tracing::info!("TSO: Counter already exists"),
        }

        let bucket = "convex_tso".to_string();
        let batch_size = batch_size.unwrap_or(DEFAULT_BATCH_SIZE);

        tracing::info!("TSO: Connected to NATS KV, batch_size={batch_size}");

        Ok(Self {
            nats_client: client,
            kv_bucket: bucket,
            batch_size,
            state: Mutex::new(BatchState {
                current: 0,
                upper_bound: 0,
                max_committed: Timestamp::MIN,
            }),
        })
    }

    /// Reserve a new batch of timestamps from the central counter.
    /// Uses NATS KV atomic update (CAS) to ensure no two nodes get
    /// overlapping ranges.
    async fn reserve_batch(&self) -> anyhow::Result<(u64, u64)> {
        let jetstream = async_nats::jetstream::new(self.nats_client.clone());
        let kv = jetstream
            .get_key_value("convex_tso")
            .await
            .context("TSO: Failed to get KV bucket")?;

        // Retry loop for CAS conflicts.
        for attempt in 0..10 {
            let entry = kv
                .entry(TSO_COUNTER_KEY)
                .await
                .context("TSO: Failed to read counter")?
                .context("TSO: Counter key not found")?;

            let current_value = u64::from_be_bytes(
                entry.value.as_ref().try_into()
                    .context("TSO: Invalid counter value")?
            );

            let new_lower = current_value;
            let new_upper = current_value + self.batch_size;
            let new_value = new_upper.to_be_bytes().to_vec();

            // Atomic compare-and-swap: only succeeds if no other node
            // modified the counter since we read it.
            match kv.update(TSO_COUNTER_KEY, new_value.into(), entry.revision).await {
                Ok(_) => {
                    tracing::info!(
                        "TSO: Reserved batch [{new_lower}, {new_upper}) on attempt {attempt}"
                    );
                    return Ok((new_lower, new_upper));
                },
                Err(_) => {
                    // Another node updated the counter. Retry.
                    tracing::debug!("TSO: CAS conflict on attempt {attempt}, retrying");
                    tokio::time::sleep(std::time::Duration::from_millis(1 << attempt)).await;
                },
            }
        }

        anyhow::bail!("TSO: Failed to reserve batch after 10 attempts")
    }
}

#[async_trait]
impl TimestampOracle for BatchTimestampOracle {
    async fn next_ts(&self) -> anyhow::Result<Timestamp> {
        // Fast path: check if we have a timestamp available in the current batch.
        {
            let mut state = self.state.lock();
            if state.current < state.upper_bound {
                let ts = state.current;
                state.current += 1;
                return Timestamp::try_from(ts);
            }
        }

        // Slow path: reserve a new batch from NATS KV.
        let (lower, upper) = self.reserve_batch().await?;

        let mut state = self.state.lock();
        state.current = lower + 1; // We'll return `lower`, advance to lower+1.
        state.upper_bound = upper;

        Timestamp::try_from(lower)
    }

    async fn max_committed_ts(&self) -> anyhow::Result<Timestamp> {
        // Read from NATS KV for cross-node consistency.
        let jetstream = async_nats::jetstream::new(self.nats_client.clone());
        let kv = jetstream
            .get_key_value("convex_tso")
            .await
            .context("TSO: Failed to get KV bucket")?;

        match kv.entry(TSO_MAX_COMMITTED_KEY).await? {
            Some(entry) => {
                let ts = u64::from_be_bytes(
                    entry.value.as_ref().try_into()
                        .context("TSO: Invalid max_committed value")?
                );
                Timestamp::try_from(ts)
            },
            None => Ok(Timestamp::MIN),
        }
    }

    async fn advance_committed_ts(&self, ts: Timestamp) -> anyhow::Result<()> {
        let ts_u64 = u64::from(ts);

        // Update local state.
        {
            let mut state = self.state.lock();
            if ts > state.max_committed {
                state.max_committed = ts;
            }
        }

        // Update NATS KV (best-effort, non-blocking for the commit path).
        let jetstream = async_nats::jetstream::new(self.nats_client.clone());
        let kv = jetstream
            .get_key_value("convex_tso")
            .await
            .context("TSO: Failed to get KV bucket")?;

        let value = ts_u64.to_be_bytes().to_vec();
        let _ = kv.put(TSO_MAX_COMMITTED_KEY, value.into()).await;

        Ok(())
    }
}

#[cfg(any(test, feature = "testing"))]
pub mod testing {
    use super::*;

    /// In-memory TSO for testing. Thread-safe atomic counter.
    pub struct InMemoryTimestampOracle {
        state: Mutex<LocalState>,
    }

    impl InMemoryTimestampOracle {
        pub fn new() -> Self {
            let start = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos() as u64;
            Self {
                state: Mutex::new(LocalState {
                    last_assigned: Timestamp::try_from(start).unwrap_or(Timestamp::MIN),
                    max_committed: Timestamp::MIN,
                }),
            }
        }
    }

    #[async_trait]
    impl TimestampOracle for InMemoryTimestampOracle {
        async fn next_ts(&self) -> anyhow::Result<Timestamp> {
            let mut state = self.state.lock();
            let next = state.last_assigned.succ()?;
            state.last_assigned = next;
            Ok(next)
        }

        async fn max_committed_ts(&self) -> anyhow::Result<Timestamp> {
            Ok(self.state.lock().max_committed)
        }

        async fn advance_committed_ts(&self, ts: Timestamp) -> anyhow::Result<()> {
            let mut state = self.state.lock();
            if ts > state.max_committed {
                state.max_committed = ts;
            }
            Ok(())
        }
    }
}
