//! Global Timestamp Oracle (TSO) for distributed timestamp assignment.
//!
//! In a partitioned-write architecture, multiple Committers run on different
//! nodes. Each needs globally unique, monotonically increasing timestamps.
//! The TSO ensures no two nodes assign the same timestamp.
//!
//! ## Design (inspired by Vitess sequence tables)
//!
//! A central counter is stored in NATS KV. Each node reserves a batch of
//! timestamps (e.g., 1000 at a time). Within the batch, the local counter can
//! advance without reserving a new range or reading the global
//! committed-timestamp floor. The oracle refreshes that floor when leadership
//! changes or when reserving a new batch from the central counter.
//!
//! This gives the performance of local timestamp assignment with the
//! correctness of global ordering.
//!
//! ## Implementations
//!
//! - [`LocalTimestampOracle`]: Wraps the existing Committer logic for
//!   single-node deployments. No behavior change.
//! - [`BatchTimestampOracle`]: Reserves batches from NATS KV for multi-node
//!   deployments.

use anyhow::Context;
use async_trait::async_trait;
use common::{
    runtime::Runtime,
    types::Timestamp,
};
use parking_lot::Mutex;

use crate::metrics;

/// Trait for assigning globally unique, monotonically increasing timestamps.
///
/// Each Committer calls `next_ts_at_or_after()` before committing a
/// transaction.
/// The implementation must guarantee that no two calls — across any node
/// in the cluster — ever return the same timestamp.
#[async_trait]
pub trait TimestampOracle: Send + Sync + 'static {
    /// Get the next globally unique timestamp.
    /// Must be monotonically increasing within each node.
    /// Must not overlap with timestamps from other nodes.
    async fn next_ts(&self) -> anyhow::Result<Timestamp> {
        self.next_ts_at_or_after(Timestamp::MIN).await
    }

    /// Get the next globally unique timestamp at or above `min_ts`.
    ///
    /// `min_ts` is the caller's local safety floor, usually derived from the
    /// local write log, restored snapshot, and last assigned timestamp. Global
    /// TSOs must include this floor in their reservation, rather than allowing
    /// callers to bump a returned timestamp outside the reserved range.
    async fn next_ts_at_or_after(&self, min_ts: Timestamp) -> anyhow::Result<Timestamp>;

    /// Assign a timestamp only when doing so requires no network I/O.
    ///
    /// Optional maintenance runs this method from the single committer loop.
    /// Batch-based implementations return `None` when their local reservation
    /// cannot satisfy the floor; callers may then request an asynchronous
    /// refill without blocking commits, reads, or replicated apply.
    fn try_next_ts_at_or_after(&self, _min_ts: Timestamp) -> anyhow::Result<Option<Timestamp>> {
        Ok(None)
    }

    /// Ensure a future nonblocking allocation can satisfy `min_ts`.
    ///
    /// This may perform network I/O and must therefore be called outside the
    /// single-threaded committer loop. Implementations that do not batch may
    /// leave the default no-op behavior.
    async fn prefetch_ts_at_or_after(&self, _min_ts: Timestamp) -> anyhow::Result<()> {
        Ok(())
    }

    /// Get the current maximum committed timestamp across all nodes.
    /// Used for read-after-write consistency.
    async fn max_committed_ts(&self) -> anyhow::Result<Timestamp>;

    /// Record a committed timestamp in node-local state without network I/O.
    ///
    /// The commit path calls this before publishing the new snapshot so future
    /// local allocations immediately respect the floor. Durable publication of
    /// the cluster-wide floor is asynchronous and handled by
    /// `advance_committed_ts`.
    fn observe_committed_ts(&self, _ts: Timestamp) {}

    /// Advance the max committed timestamp. Called after a successful commit.
    async fn advance_committed_ts(&self, ts: Timestamp) -> anyhow::Result<()>;

    /// Discard any node-local timestamp reservation after a leadership epoch
    /// change.
    ///
    /// Batch-based TSOs must not carry an old local reservation across Raft
    /// leadership changes. A re-elected leader should fence itself against the
    /// latest global counter and committed floor before assigning more commit
    /// timestamps.
    fn discard_reserved_batch(&self) {}
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
    async fn next_ts_at_or_after(&self, min_ts: Timestamp) -> anyhow::Result<Timestamp> {
        let mut state = self.state.lock();
        let system_ts = self.runtime.generate_timestamp()?;
        let next = std::cmp::max(
            std::cmp::max(system_ts, min_ts),
            std::cmp::max(state.last_assigned.succ()?, state.max_committed.succ()?),
        );
        state.last_assigned = next;
        Ok(next)
    }

    fn try_next_ts_at_or_after(&self, min_ts: Timestamp) -> anyhow::Result<Option<Timestamp>> {
        let mut state = self.state.lock();
        let system_ts = self.runtime.generate_timestamp()?;
        let next = std::cmp::max(
            std::cmp::max(system_ts, min_ts),
            std::cmp::max(state.last_assigned.succ()?, state.max_committed.succ()?),
        );
        state.last_assigned = next;
        Ok(Some(next))
    }

    async fn max_committed_ts(&self) -> anyhow::Result<Timestamp> {
        Ok(self.state.lock().max_committed)
    }

    fn observe_committed_ts(&self, ts: Timestamp) {
        let mut state = self.state.lock();
        if ts > state.max_committed {
            state.max_committed = ts;
        }
    }

    async fn advance_committed_ts(&self, ts: Timestamp) -> anyhow::Result<()> {
        self.observe_committed_ts(ts);
        Ok(())
    }
}

/// Batch timestamp oracle for multi-node deployments.
///
/// Reserves ranges of timestamps from a central NATS KV counter.
/// Within a range, timestamps can advance without reserving a new range.
/// Each batch reservation refreshes the global committed-timestamp floor so a
/// node fences stale leadership epochs and starts new ranges above the latest
/// known cluster watermark. Once a batch is reserved, allocation within the
/// range is local.
///
/// Example with batch_size=1000:
/// - Node A reserves [1000, 1999]
/// - Node B reserves [2000, 2999]
/// - Node A assigns 1000, 1001, 1002... locally
/// - Node B assigns 2000, 2001, 2002... locally
/// - No overlap, no coordination within a batch
pub struct BatchTimestampOracle {
    kv: async_nats::jetstream::kv::Store,
    batch_size: u64,
    state: Mutex<BatchState>,
    reservation_lock: tokio::sync::Mutex<()>,
}

struct BatchState {
    /// Current position within the reserved range.
    current: u64,
    /// Upper bound (exclusive) of the reserved range.
    upper_bound: u64,
    /// Maximum committed timestamp seen.
    max_committed: Timestamp,
    /// Force the next assignment to refresh the global committed floor before
    /// reserving or consuming any batch.
    refresh_committed_floor: bool,
    /// Invalidates a reservation that was in flight when Raft leadership
    /// changed. Such a range is safely leaked rather than installed in a newer
    /// leadership epoch.
    reservation_epoch: u64,
}

const TSO_COUNTER_KEY: &str = "tso_counter";
const TSO_MAX_COMMITTED_KEY: &str = "tso_max_committed";
const DEFAULT_BATCH_SIZE: u64 = 1000;

fn next_ts_from_reserved_batch(
    current: u64,
    upper_bound: u64,
    min_next: u64,
) -> Option<(u64, u64)> {
    let candidate = std::cmp::max(current, min_next);
    (candidate < upper_bound).then_some((candidate, candidate + 1))
}

fn batch_lower_bound(counter_value: u64, min_next: u64) -> u64 {
    std::cmp::max(counter_value, min_next)
}

fn reserved_batch_can_satisfy_local_floor(
    current: u64,
    upper_bound: u64,
    local_min_next: u64,
) -> bool {
    next_ts_from_reserved_batch(current, upper_bound, local_min_next).is_some()
}

fn local_min_next_from_floor(min_ts: Timestamp, max_committed: Timestamp) -> anyhow::Result<u64> {
    Ok(u64::from(std::cmp::max(min_ts, max_committed.succ()?)))
}

fn discard_batch_state_for_leadership_change(state: &mut BatchState) {
    state.current = 0;
    state.upper_bound = 0;
    state.refresh_committed_floor = true;
    state.reservation_epoch = state.reservation_epoch.wrapping_add(1);
}

fn refresh_committed_floor(state: &mut BatchState, committed_floor: Timestamp) {
    if committed_floor > state.max_committed {
        state.max_committed = committed_floor;
    }
    state.refresh_committed_floor = false;
}

impl BatchTimestampOracle {
    /// Connect to NATS and initialize the KV bucket for timestamp allocation.
    pub async fn connect(nats_url: &str, batch_size: Option<u64>) -> anyhow::Result<Self> {
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
        match kv
            .create(TSO_COUNTER_KEY, initial_ts.to_be_bytes().to_vec().into())
            .await
        {
            Ok(_) => tracing::info!("TSO: Initialized counter at {initial_ts}"),
            Err(_) => tracing::info!("TSO: Counter already exists"),
        }

        let batch_size = batch_size.unwrap_or(DEFAULT_BATCH_SIZE);

        tracing::info!("TSO: Connected to NATS KV, batch_size={batch_size}");

        Ok(Self {
            kv,
            batch_size,
            state: Mutex::new(BatchState {
                current: 0,
                upper_bound: 0,
                max_committed: Timestamp::MIN,
                refresh_committed_floor: false,
                reservation_epoch: 0,
            }),
            reservation_lock: tokio::sync::Mutex::new(()),
        })
    }

    /// Reserve a new batch of timestamps from the central counter.
    /// Uses NATS KV atomic update (CAS) to ensure no two nodes get
    /// overlapping ranges.
    async fn reserve_batch_at_or_above(&self, min_next: u64) -> anyhow::Result<(u64, u64)> {
        let timer = metrics::tso_operation_timer("batch_reserve");
        // Retry loop for CAS conflicts.
        for attempt in 0..10 {
            let entry = self
                .kv
                .entry(TSO_COUNTER_KEY)
                .await
                .context("TSO: Failed to read counter")?
                .context("TSO: Counter key not found")?;

            let current_value = u64::from_be_bytes(
                entry
                    .value
                    .as_ref()
                    .try_into()
                    .context("TSO: Invalid counter value")?,
            );

            let new_lower = batch_lower_bound(current_value, min_next);
            let new_upper = new_lower
                .checked_add(self.batch_size)
                .context("TSO: Reserved batch upper bound overflow")?;
            let new_value = new_upper.to_be_bytes().to_vec();

            // Atomic compare-and-swap: only succeeds if no other node
            // modified the counter since we read it.
            match self
                .kv
                .update(TSO_COUNTER_KEY, new_value.into(), entry.revision)
                .await
            {
                Ok(_) => {
                    tracing::info!(
                        "TSO: Reserved batch [{new_lower}, {new_upper}) on attempt {attempt}"
                    );
                    timer.finish();
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

    fn try_assign_from_reserved_batch(
        &self,
        min_ts: Timestamp,
    ) -> anyhow::Result<Option<Timestamp>> {
        let mut state = self.state.lock();
        if state.refresh_committed_floor {
            return Ok(None);
        }
        let min_next = local_min_next_from_floor(min_ts, state.max_committed)?;
        let Some((ts, next_current)) =
            next_ts_from_reserved_batch(state.current, state.upper_bound, min_next)
        else {
            return Ok(None);
        };
        state.current = next_current;
        Ok(Some(Timestamp::try_from(ts)?))
    }

    async fn ensure_reserved_batch_at_or_above(&self, min_ts: Timestamp) -> anyhow::Result<()> {
        let _reservation_guard = self.reservation_lock.lock().await;
        loop {
            if self.try_assignable_from_reserved_batch(min_ts)? {
                return Ok(());
            }

            let committed_floor = self.max_committed_ts().await?;
            let (min_next, reservation_epoch) = {
                let mut state = self.state.lock();
                refresh_committed_floor(&mut state, committed_floor);
                let min_next = local_min_next_from_floor(min_ts, state.max_committed)?;
                if reserved_batch_can_satisfy_local_floor(
                    state.current,
                    state.upper_bound,
                    min_next,
                ) {
                    return Ok(());
                }
                (min_next, state.reservation_epoch)
            };

            let (lower, upper) = self.reserve_batch_at_or_above(min_next).await?;
            let mut state = self.state.lock();
            if state.reservation_epoch != reservation_epoch {
                // Leadership changed while NATS was reserving this range. Do not
                // install it into the newer epoch; loop and fence against the
                // latest committed floor instead.
                continue;
            }
            state.current = lower;
            state.upper_bound = upper;
            return Ok(());
        }
    }

    fn try_assignable_from_reserved_batch(&self, min_ts: Timestamp) -> anyhow::Result<bool> {
        let state = self.state.lock();
        if state.refresh_committed_floor {
            return Ok(false);
        }
        let min_next = local_min_next_from_floor(min_ts, state.max_committed)?;
        Ok(reserved_batch_can_satisfy_local_floor(
            state.current,
            state.upper_bound,
            min_next,
        ))
    }
}

#[async_trait]
impl TimestampOracle for BatchTimestampOracle {
    async fn next_ts_at_or_after(&self, min_ts: Timestamp) -> anyhow::Result<Timestamp> {
        let timer = metrics::tso_operation_timer("next_ts_at_or_after");
        let result = async {
            loop {
                if let Some(ts) = self.try_assign_from_reserved_batch(min_ts)? {
                    return Ok(ts);
                }
                self.ensure_reserved_batch_at_or_above(min_ts).await?;
            }
        }
        .await;
        if result.is_ok() {
            timer.finish();
        }
        result
    }

    fn try_next_ts_at_or_after(&self, min_ts: Timestamp) -> anyhow::Result<Option<Timestamp>> {
        self.try_assign_from_reserved_batch(min_ts)
    }

    async fn prefetch_ts_at_or_after(&self, min_ts: Timestamp) -> anyhow::Result<()> {
        self.ensure_reserved_batch_at_or_above(min_ts).await
    }

    async fn max_committed_ts(&self) -> anyhow::Result<Timestamp> {
        let timer = metrics::tso_operation_timer("max_committed_read");
        let result = async {
            match self.kv.entry(TSO_MAX_COMMITTED_KEY).await? {
                Some(entry) => {
                    let ts = u64::from_be_bytes(
                        entry
                            .value
                            .as_ref()
                            .try_into()
                            .context("TSO: Invalid max_committed value")?,
                    );
                    Timestamp::try_from(ts)
                },
                None => Ok(Timestamp::MIN),
            }
        }
        .await;
        if result.is_ok() {
            timer.finish();
        }
        result
    }

    fn observe_committed_ts(&self, ts: Timestamp) {
        let mut state = self.state.lock();
        if ts > state.max_committed {
            state.max_committed = ts;
        }
    }

    async fn advance_committed_ts(&self, ts: Timestamp) -> anyhow::Result<()> {
        let timer = metrics::tso_operation_timer("advance_committed");
        let result = async {
            let ts_u64 = u64::from(ts);

            self.observe_committed_ts(ts);

            let value = ts_u64.to_be_bytes().to_vec();
            for attempt in 0..10 {
                match self.kv.entry(TSO_MAX_COMMITTED_KEY).await? {
                    Some(entry) => {
                        let current = u64::from_be_bytes(
                            entry
                                .value
                                .as_ref()
                                .try_into()
                                .context("TSO: Invalid max_committed value")?,
                        );
                        if current >= ts_u64 {
                            return Ok(());
                        }

                        match self
                            .kv
                            .update(TSO_MAX_COMMITTED_KEY, value.clone().into(), entry.revision)
                            .await
                        {
                            Ok(_) => return Ok(()),
                            Err(_) => {
                                tracing::debug!(
                                    "TSO: max_committed CAS conflict on attempt {attempt}, \
                                     retrying"
                                );
                                tokio::time::sleep(std::time::Duration::from_millis(1 << attempt))
                                    .await;
                            },
                        }
                    },
                    None => match self
                        .kv
                        .create(TSO_MAX_COMMITTED_KEY, value.clone().into())
                        .await
                    {
                        Ok(_) => return Ok(()),
                        Err(_) => {
                            tracing::debug!(
                                "TSO: max_committed create conflict on attempt {attempt}, retrying"
                            );
                            tokio::time::sleep(std::time::Duration::from_millis(1 << attempt))
                                .await;
                        },
                    },
                }
            }

            anyhow::bail!("TSO: Failed to advance max_committed after 10 attempts")
        }
        .await;
        if result.is_ok() {
            timer.finish();
        }
        result
    }

    fn discard_reserved_batch(&self) {
        let mut state = self.state.lock();
        discard_batch_state_for_leadership_change(&mut state);
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
        async fn next_ts_at_or_after(&self, min_ts: Timestamp) -> anyhow::Result<Timestamp> {
            let mut state = self.state.lock();
            let next = std::cmp::max(
                min_ts,
                std::cmp::max(state.last_assigned.succ()?, state.max_committed.succ()?),
            );
            state.last_assigned = next;
            Ok(next)
        }

        fn try_next_ts_at_or_after(&self, min_ts: Timestamp) -> anyhow::Result<Option<Timestamp>> {
            let mut state = self.state.lock();
            let next = std::cmp::max(
                min_ts,
                std::cmp::max(state.last_assigned.succ()?, state.max_committed.succ()?),
            );
            state.last_assigned = next;
            Ok(Some(next))
        }

        async fn max_committed_ts(&self) -> anyhow::Result<Timestamp> {
            Ok(self.state.lock().max_committed)
        }

        fn observe_committed_ts(&self, ts: Timestamp) {
            let mut state = self.state.lock();
            if ts > state.max_committed {
                state.max_committed = ts;
            }
        }

        async fn advance_committed_ts(&self, ts: Timestamp) -> anyhow::Result<()> {
            self.observe_committed_ts(ts);
            Ok(())
        }
    }
}

#[cfg(test)]
mod tests {
    use common::types::Timestamp;

    use super::{
        batch_lower_bound,
        discard_batch_state_for_leadership_change,
        local_min_next_from_floor,
        next_ts_from_reserved_batch,
        refresh_committed_floor,
        reserved_batch_can_satisfy_local_floor,
        BatchState,
    };

    #[test]
    fn reserved_batch_uses_current_when_already_above_floor() {
        let (ts, next_current) =
            next_ts_from_reserved_batch(100, 200, 90).expect("candidate within batch");
        assert_eq!(ts, 100);
        assert_eq!(next_current, 101);
    }

    #[test]
    fn reserved_batch_fast_forwards_to_requested_floor() {
        let (ts, next_current) =
            next_ts_from_reserved_batch(100, 200, 150).expect("candidate within batch");
        assert_eq!(ts, 150);
        assert_eq!(next_current, 151);
    }

    #[test]
    fn reserved_batch_exhausts_when_floor_is_beyond_range() {
        assert!(next_ts_from_reserved_batch(100, 200, 200).is_none());
        assert!(next_ts_from_reserved_batch(100, 200, 250).is_none());
    }

    #[test]
    fn reserve_batch_starts_above_requested_floor_when_counter_lags() {
        assert_eq!(batch_lower_bound(1000, 1001), 1001);
        assert_eq!(batch_lower_bound(1000, 1500), 1500);
        assert_eq!(batch_lower_bound(2000, 1500), 2000);
    }

    #[test]
    fn reserved_batch_can_assign_locally_when_it_satisfies_floor() {
        assert!(reserved_batch_can_satisfy_local_floor(100, 200, 150));
    }

    #[test]
    fn reserved_batch_cannot_assign_locally_when_missing_or_exhausted() {
        assert!(!reserved_batch_can_satisfy_local_floor(0, 0, 1));
        assert!(!reserved_batch_can_satisfy_local_floor(100, 200, 200));
        assert!(!reserved_batch_can_satisfy_local_floor(100, 200, 250));
    }

    #[test]
    fn leadership_change_discards_reserved_batch() {
        let mut state = BatchState {
            current: 150,
            upper_bound: 200,
            max_committed: Timestamp::must(120),
            refresh_committed_floor: false,
            reservation_epoch: 0,
        };

        discard_batch_state_for_leadership_change(&mut state);

        assert_eq!(state.current, 0);
        assert_eq!(state.upper_bound, 0);
        assert_eq!(state.max_committed, Timestamp::must(120));
        assert!(state.refresh_committed_floor);
    }

    #[test]
    fn leadership_floor_refresh_prevents_stale_batch_reuse() -> anyhow::Result<()> {
        let mut state = BatchState {
            current: 150,
            upper_bound: 200,
            max_committed: Timestamp::must(120),
            refresh_committed_floor: false,
            reservation_epoch: 0,
        };

        discard_batch_state_for_leadership_change(&mut state);
        refresh_committed_floor(&mut state, Timestamp::must(250));

        let min_next = local_min_next_from_floor(Timestamp::MIN, state.max_committed)?;

        assert_eq!(min_next, 251);
        assert!(!state.refresh_committed_floor);
        assert!(
            next_ts_from_reserved_batch(state.current, state.upper_bound, min_next).is_none(),
            "a re-elected leader must not assign from the old leadership epoch's local batch",
        );
        assert_eq!(batch_lower_bound(200, min_next), 251);
        Ok(())
    }
}
