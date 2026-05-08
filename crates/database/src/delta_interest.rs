use std::{
    collections::{
        BTreeMap,
        BTreeSet,
    },
    sync::Arc,
    time::{
        Duration,
        Instant,
    },
};

use parking_lot::Mutex;
use tokio::sync::watch;
use value::TableName;

use crate::metrics;

#[derive(Clone)]
pub struct DeltaInterestTracker {
    inner: Arc<Mutex<InterestState>>,
    tx: watch::Sender<Arc<BTreeSet<TableName>>>,
}

#[derive(Default)]
struct InterestState {
    ref_counts: BTreeMap<TableName, usize>,
    recent_expirations: BTreeMap<TableName, Instant>,
}

impl DeltaInterestTracker {
    pub fn new() -> Self {
        let initial = Arc::new(BTreeSet::new());
        let (tx, _) = watch::channel(initial);
        Self {
            inner: Arc::new(Mutex::new(InterestState::default())),
            tx,
        }
    }

    pub fn add_tables(&self, tables: &BTreeSet<TableName>) {
        if tables.is_empty() {
            return;
        }
        let mut state = self.inner.lock();
        state.prune_expired(Instant::now());
        for table in tables {
            *state.ref_counts.entry(table.clone()).or_default() += 1;
        }
        self.publish_locked(&state);
    }

    pub fn remove_tables(&self, tables: &BTreeSet<TableName>) {
        if tables.is_empty() {
            return;
        }
        let mut state = self.inner.lock();
        state.prune_expired(Instant::now());
        for table in tables {
            let Some(count) = state.ref_counts.get_mut(table) else {
                continue;
            };
            if *count <= 1 {
                state.ref_counts.remove(table);
            } else {
                *count -= 1;
            }
        }
        self.publish_locked(&state);
    }

    pub fn refresh_recent_tables(&self, tables: &BTreeSet<TableName>, ttl: Duration) {
        self.refresh_recent_tables_at(tables, Instant::now(), ttl);
    }

    pub fn prune_expired(&self) {
        self.prune_expired_at(Instant::now());
    }

    pub fn snapshot(&self) -> Arc<BTreeSet<TableName>> {
        self.prune_expired();
        self.tx.borrow().clone()
    }

    pub fn watch(&self) -> watch::Receiver<Arc<BTreeSet<TableName>>> {
        self.tx.subscribe()
    }

    fn refresh_recent_tables_at(&self, tables: &BTreeSet<TableName>, now: Instant, ttl: Duration) {
        if tables.is_empty() || ttl.is_zero() {
            return;
        }
        let Some(expires_at) = now.checked_add(ttl) else {
            return;
        };
        let mut state = self.inner.lock();
        state.prune_expired(now);
        for table in tables {
            state
                .recent_expirations
                .entry(table.clone())
                .and_modify(|current| *current = (*current).max(expires_at))
                .or_insert(expires_at);
        }
        self.publish_locked(&state);
    }

    fn prune_expired_at(&self, now: Instant) {
        let mut state = self.inner.lock();
        if state.prune_expired(now) {
            self.publish_locked(&state);
        }
    }

    fn publish_locked(&self, state: &InterestState) {
        let snapshot: Arc<BTreeSet<TableName>> = Arc::new(
            state
                .ref_counts
                .keys()
                .chain(state.recent_expirations.keys())
                .cloned()
                .collect(),
        );
        metrics::log_selective_delivery_interested_tables(snapshot.len());
        self.tx.send_replace(snapshot);
    }
}

impl InterestState {
    fn prune_expired(&mut self, now: Instant) -> bool {
        let before = self.recent_expirations.len();
        self.recent_expirations.retain(|_, expiry| *expiry > now);
        before != self.recent_expirations.len()
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use maplit::btreeset;
    use value::TableName;

    use crate::delta_interest::DeltaInterestTracker;

    #[test]
    fn interest_tracker_ref_counts_tables() {
        let tracker = DeltaInterestTracker::new();
        let tasks: TableName = "tasks".parse().unwrap();
        let messages: TableName = "messages".parse().unwrap();

        tracker.add_tables(&btreeset! { messages.clone(), tasks.clone() });
        tracker.add_tables(&btreeset! { tasks.clone() });
        assert_eq!(
            tracker.snapshot().as_ref(),
            &btreeset! { messages.clone(), tasks.clone() }
        );

        tracker.remove_tables(&btreeset! { tasks.clone() });
        assert_eq!(
            tracker.snapshot().as_ref(),
            &btreeset! { messages.clone(), tasks.clone() }
        );

        tracker.remove_tables(&btreeset! { messages.clone(), tasks.clone() });
        assert_eq!(tracker.snapshot().as_ref(), &btreeset! {});
    }

    #[test]
    fn interest_tracker_recent_tables_expire() {
        let tracker = DeltaInterestTracker::new();
        let tasks: TableName = "tasks".parse().unwrap();
        let now = std::time::Instant::now();

        tracker.refresh_recent_tables_at(
            &btreeset! { tasks.clone() },
            now,
            Duration::from_secs(30),
        );
        assert_eq!(tracker.snapshot().as_ref(), &btreeset! { tasks.clone() });

        tracker.prune_expired_at(now + Duration::from_secs(29));
        assert_eq!(tracker.snapshot().as_ref(), &btreeset! { tasks.clone() });

        tracker.prune_expired_at(now + Duration::from_secs(30));
        assert_eq!(tracker.snapshot().as_ref(), &btreeset! {});
    }
}
