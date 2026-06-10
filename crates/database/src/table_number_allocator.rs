//! Table number allocation for user tables.
//!
//! In single-node mode Convex can choose the next available table number from
//! the local `_tables` snapshot. In clustered mode public document IDs need to
//! be portable across nodes, so table numbers must come from one global
//! allocator instead of each node assigning from its own local namespace.

use std::time::Duration;

use anyhow::Context;
use async_trait::async_trait;
use value::TableNumber;

use crate::bootstrap_model::table::NUM_RESERVED_SYSTEM_TABLE_NUMBERS;

#[async_trait]
pub trait TableNumberAllocator: Send + Sync + 'static {
    async fn next_user_table_number(&self, local_floor: TableNumber)
        -> anyhow::Result<TableNumber>;
}

#[derive(Clone, Copy, Debug, Default)]
pub struct LocalTableNumberAllocator;

#[async_trait]
impl TableNumberAllocator for LocalTableNumberAllocator {
    async fn next_user_table_number(
        &self,
        local_floor: TableNumber,
    ) -> anyhow::Result<TableNumber> {
        Ok(local_floor)
    }
}

#[derive(Clone)]
pub struct NatsTableNumberAllocator {
    kv: async_nats::jetstream::kv::Store,
}

const TABLE_NUMBER_KV_BUCKET: &str = "convex_table_numbers";
const USER_TABLE_NUMBER_COUNTER_KEY: &str = "user_table_number_counter";

fn first_user_table_number() -> anyhow::Result<TableNumber> {
    TableNumber::try_from(NUM_RESERVED_SYSTEM_TABLE_NUMBERS)?.increment()
}

impl NatsTableNumberAllocator {
    pub async fn connect(nats_url: &str) -> anyhow::Result<Self> {
        let _ = rustls::crypto::ring::default_provider().install_default();
        let client = async_nats::connect(nats_url)
            .await
            .with_context(|| format!("TableNumberAllocator: Failed to connect to {nats_url}"))?;
        let jetstream = async_nats::jetstream::new(client);
        let kv = jetstream
            .create_key_value(async_nats::jetstream::kv::Config {
                bucket: TABLE_NUMBER_KV_BUCKET.to_string(),
                history: 1,
                ..Default::default()
            })
            .await
            .context("TableNumberAllocator: Failed to create KV bucket")?;

        let initial = u32::from(first_user_table_number()?);
        match kv
            .create(
                USER_TABLE_NUMBER_COUNTER_KEY,
                initial.to_be_bytes().to_vec().into(),
            )
            .await
        {
            Ok(_) => tracing::info!("TableNumberAllocator: initialized counter at {initial}"),
            Err(_) => tracing::info!("TableNumberAllocator: counter already exists"),
        }

        Ok(Self { kv })
    }
}

#[async_trait]
impl TableNumberAllocator for NatsTableNumberAllocator {
    async fn next_user_table_number(
        &self,
        local_floor: TableNumber,
    ) -> anyhow::Result<TableNumber> {
        let local_floor = u32::from(local_floor);

        for attempt in 0..10 {
            let entry = self
                .kv
                .entry(USER_TABLE_NUMBER_COUNTER_KEY)
                .await
                .context("TableNumberAllocator: Failed to read counter")?
                .context("TableNumberAllocator: Counter key not found")?;
            let counter = u32::from_be_bytes(
                entry
                    .value
                    .as_ref()
                    .try_into()
                    .context("TableNumberAllocator: Invalid counter value")?,
            );
            let candidate = counter.max(local_floor);
            let next = candidate
                .checked_add(1)
                .context("TableNumberAllocator: table number overflow")?;

            match self
                .kv
                .update(
                    USER_TABLE_NUMBER_COUNTER_KEY,
                    next.to_be_bytes().to_vec().into(),
                    entry.revision,
                )
                .await
            {
                Ok(_) => {
                    tracing::info!(
                        "TableNumberAllocator: allocated user table number {candidate} on attempt \
                         {attempt}"
                    );
                    return TableNumber::try_from(candidate);
                },
                Err(_) => {
                    tracing::debug!(
                        "TableNumberAllocator: CAS conflict on attempt {attempt}, retrying"
                    );
                    tokio::time::sleep(Duration::from_millis(1 << attempt)).await;
                },
            }
        }

        anyhow::bail!("TableNumberAllocator: Failed to allocate after 10 attempts")
    }
}

#[cfg(test)]
pub mod testing {
    use std::sync::atomic::{
        AtomicU32,
        Ordering,
    };

    use async_trait::async_trait;
    use value::TableNumber;

    use crate::{
        bootstrap_model::table::NUM_RESERVED_SYSTEM_TABLE_NUMBERS,
        table_number_allocator::TableNumberAllocator,
    };

    #[derive(Debug)]
    pub struct InMemoryTableNumberAllocator {
        next: AtomicU32,
    }

    impl Default for InMemoryTableNumberAllocator {
        fn default() -> Self {
            Self {
                next: AtomicU32::new(NUM_RESERVED_SYSTEM_TABLE_NUMBERS + 1),
            }
        }
    }

    #[async_trait]
    impl TableNumberAllocator for InMemoryTableNumberAllocator {
        async fn next_user_table_number(
            &self,
            local_floor: TableNumber,
        ) -> anyhow::Result<TableNumber> {
            let local_floor = u32::from(local_floor);
            loop {
                let current = self.next.load(Ordering::SeqCst);
                let candidate = current.max(local_floor);
                let next = candidate.checked_add(1).ok_or_else(|| {
                    anyhow::anyhow!("TableNumberAllocator: table number overflow")
                })?;
                match self
                    .next
                    .compare_exchange(current, next, Ordering::SeqCst, Ordering::SeqCst)
                {
                    Ok(_) => return TableNumber::try_from(candidate),
                    Err(_) => continue,
                }
            }
        }
    }
}
