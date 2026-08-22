use std::{
    collections::{
        BTreeMap,
        BTreeSet,
        VecDeque,
    },
    error::Error,
    fmt,
    sync::Arc,
    time::{
        Duration,
        Instant,
    },
};

use parking_lot::Mutex;
use serde::{
    Deserialize,
    Serialize,
};
use tokio::sync::watch;
use value::TableName;

use crate::{
    metrics,
    reads::ReadSet,
};

/// Wire contract version for distributed reactive invalidation ownership.
///
/// Runtime selective delivery still uses the table-level registry below. These
/// types deliberately remain a correctness primitive until owner registration,
/// durable replay, and delivery are integrated end to end.
pub const INVALIDATION_OWNERSHIP_PROTOCOL_VERSION: u16 = 1;

/// Stable identity of one invalidation-matching owner.
///
/// This is a string rather than a partition id because future catalog versions
/// may split one partition's matching responsibility without changing this
/// protocol.
#[derive(Clone, Debug, Eq, Ord, PartialEq, PartialOrd, Serialize, Deserialize)]
pub struct InvalidationOwnerId(pub String);

/// Stable identity of one logical subscription registration.
#[derive(Clone, Debug, Eq, Ord, PartialEq, PartialOrd, Serialize, Deserialize)]
pub struct InvalidationRegistrationId(pub String);

/// Cursor in the globally ordered committed-write domain.
#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd, Serialize, Deserialize)]
pub struct InvalidationCursor(pub u64);

/// An index name in the ownership protocol.
#[derive(Clone, Debug, Eq, Ord, PartialEq, PartialOrd, Serialize, Deserialize)]
pub struct InvalidationIndex {
    /// Stable tablet identity from the catalog version named by the
    /// registration.
    pub tablet_id: String,
    pub descriptor: String,
}

/// A query dependency registered with an invalidation owner.
///
/// Search dependencies and full-index reads are widened to `TableScan` by
/// `dependencies_from_read_set`. This may produce false positives, but never a
/// false negative.
#[derive(Clone, Debug, Eq, Ord, PartialEq, PartialOrd, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum InvalidationDependency {
    Point {
        index: InvalidationIndex,
        key: Vec<u8>,
    },
    IndexRange {
        index: InvalidationIndex,
        start_inclusive: Vec<u8>,
        end_exclusive: Option<Vec<u8>>,
    },
    TableScan {
        tablet_id: String,
    },
}

impl InvalidationDependency {
    fn tablet_id(&self) -> &str {
        match self {
            Self::Point { index, .. } | Self::IndexRange { index, .. } => &index.tablet_id,
            Self::TableScan { tablet_id } => tablet_id,
        }
    }

    fn matches(&self, write: &InvalidationWrite) -> bool {
        if self.tablet_id() != write.tablet_id {
            return false;
        }
        match self {
            Self::TableScan { .. } => true,
            Self::Point { index, key } => {
                !write.index_keys_complete
                    || write
                        .index_keys
                        .get(&index.descriptor)
                        // Missing key material cannot prove non-overlap, so invalidate.
                        .is_none_or(|keys| keys.iter().any(|candidate| candidate == key))
            },
            Self::IndexRange {
                index,
                start_inclusive,
                end_exclusive,
            } => {
                !write.index_keys_complete
                    || write.index_keys.get(&index.descriptor).is_none_or(|keys| {
                        keys.iter().any(|key| {
                            key >= start_inclusive
                                && end_exclusive
                                    .as_ref()
                                    .is_none_or(|end_exclusive| key < end_exclusive)
                        })
                    })
            },
        }
    }
}

/// Convert a Convex read set into ownership-protocol dependencies.
///
/// Standard index intervals retain their precision. Singleton intervals become
/// point dependencies. Full index reads and search reads become table scans so
/// the distributed matcher cannot miss an invalidation it cannot evaluate
/// precisely yet.
pub fn dependencies_from_read_set(read_set: &ReadSet) -> BTreeSet<InvalidationDependency> {
    use common::interval::{
        End,
        Interval,
    };

    let mut dependencies = BTreeSet::new();
    let mut scanned_tablets = BTreeSet::new();
    for (index_name, reads) in read_set.iter_indexed() {
        let tablet_id = index_name.table().to_string();
        let index = InvalidationIndex {
            tablet_id: tablet_id.clone(),
            descriptor: index_name.descriptor().to_string(),
        };
        for interval in reads.intervals.iter() {
            if interval == Interval::all() {
                scanned_tablets.insert(tablet_id.clone());
            } else if let Some(point) = interval.is_singleton() {
                dependencies.insert(InvalidationDependency::Point {
                    index: index.clone(),
                    key: point.as_slice().to_vec(),
                });
            } else {
                dependencies.insert(InvalidationDependency::IndexRange {
                    index: index.clone(),
                    start_inclusive: interval.start.0.as_slice().to_vec(),
                    end_exclusive: match interval.end {
                        End::Excluded(end) => Some(end.into()),
                        End::Unbounded => None,
                    },
                });
            }
        }
    }
    for (index_name, _) in read_set.iter_search() {
        scanned_tablets.insert(index_name.table().to_string());
    }
    dependencies.retain(|dependency| !scanned_tablets.contains(dependency.tablet_id()));
    dependencies.extend(
        scanned_tablets
            .into_iter()
            .map(|tablet_id| InvalidationDependency::TableScan { tablet_id }),
    );
    dependencies
}

/// Registration sent to exactly one invalidation owner.
///
/// `catalog_version` is copied from the existing catalog/placement authority.
/// This subsystem never allocates or advances it.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct InvalidationRegistration {
    pub protocol_version: u16,
    pub registration_id: InvalidationRegistrationId,
    /// Monotonic incarnation of this logical registration id.
    pub generation: u64,
    pub owner_id: InvalidationOwnerId,
    pub catalog_version: u64,
    pub evaluation_cursor: InvalidationCursor,
    pub dependencies: BTreeSet<InvalidationDependency>,
}

/// Generation-aware removal of one logical subscription registration.
///
/// Owners retain this message as a tombstone. A delayed registration or
/// unregistration at an older generation therefore cannot resurrect or remove
/// a newer incarnation.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct InvalidationUnregistration {
    pub protocol_version: u16,
    pub registration_id: InvalidationRegistrationId,
    pub generation: u64,
    pub owner_id: InvalidationOwnerId,
    pub catalog_version: u64,
}

/// One table write and its old/new index keys.
///
/// `index_keys_complete` certifies that `index_keys` includes both old and new
/// keys for every known index under this event's catalog version. If it is
/// false, all point/range dependencies on this table match conservatively.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct InvalidationWrite {
    pub tablet_id: String,
    pub index_keys_complete: bool,
    pub index_keys: BTreeMap<String, BTreeSet<Vec<u8>>>,
}

/// Stable identity of one canonical committed event in an owner's sequence.
#[derive(Clone, Debug, Eq, Ord, PartialEq, PartialOrd, Serialize, Deserialize)]
pub struct InvalidationEventId {
    pub owner_id: InvalidationOwnerId,
    pub owner_sequence: u64,
}

/// A committed write event evaluated by one invalidation owner.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct InvalidationEvent {
    pub protocol_version: u16,
    pub event_id: InvalidationEventId,
    /// Must equal the owner's last applied sequence.
    pub predecessor_owner_sequence: u64,
    /// Global visibility cursor; deliberately separate from owner-local order.
    pub commit_cursor: InvalidationCursor,
    pub catalog_version: u64,
    pub writes: Vec<InvalidationWrite>,
}

impl InvalidationEvent {
    fn matches(&self, registration: &InvalidationRegistration) -> bool {
        registration
            .dependencies
            .iter()
            .any(|dependency| self.writes.iter().any(|write| dependency.matches(write)))
    }
}

/// Deduplicatable identity delivered to a subscription host.
#[derive(Clone, Debug, Eq, Ord, PartialEq, PartialOrd, Serialize, Deserialize)]
pub struct InvalidationIdentity {
    pub registration_id: InvalidationRegistrationId,
    pub registration_generation: u64,
    pub event_id: InvalidationEventId,
    pub commit_cursor: InvalidationCursor,
}

/// Exact interval validated while atomically installing a registration.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RegistrationReplay {
    /// Query evaluation included all writes at or before this cursor.
    pub from_exclusive: InvalidationCursor,
    /// Owner registration was linearized after replaying through this cursor.
    pub through_inclusive: InvalidationCursor,
    pub through_owner_sequence_inclusive: u64,
    pub invalidations: Vec<InvalidationIdentity>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum InvalidationRegistryError {
    UnsupportedProtocol {
        received: u16,
    },
    WrongOwner,
    CatalogVersionMismatch {
        expected: u64,
        received: u64,
    },
    OwnerBehind {
        owner_cursor: InvalidationCursor,
        evaluation_cursor: InvalidationCursor,
    },
    ReplayGap {
        retained_from_exclusive: InvalidationCursor,
        evaluation_cursor: InvalidationCursor,
    },
    StaleRegistrationGeneration {
        current: u64,
        received: u64,
    },
    RegistrationGenerationCollision {
        generation: u64,
    },
    EventIdentityCollision {
        owner_sequence: u64,
    },
    EventOutsideReplayWindow {
        owner_sequence: u64,
        retained_from_owner_sequence_exclusive: u64,
    },
    OwnerSequenceGap {
        expected_sequence: u64,
        received_sequence: u64,
        expected_predecessor: u64,
        received_predecessor: u64,
    },
    ReorderedCommitCursor {
        owner_cursor: InvalidationCursor,
        event_cursor: InvalidationCursor,
    },
}

impl fmt::Display for InvalidationRegistryError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{self:?}")
    }
}

impl Error for InvalidationRegistryError {}

#[derive(Clone, Debug, Eq, PartialEq)]
enum InvalidationRegistrationState {
    Active(InvalidationRegistration),
    Tombstone(InvalidationUnregistration),
}

impl InvalidationRegistrationState {
    fn generation(&self) -> u64 {
        match self {
            Self::Active(registration) => registration.generation,
            Self::Tombstone(unregistration) => unregistration.generation,
        }
    }

    fn active(&self) -> Option<&InvalidationRegistration> {
        match self {
            Self::Active(registration) => Some(registration),
            Self::Tombstone(_) => None,
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct RetainedInvalidationEvent {
    event: InvalidationEvent,
    /// Exact ordered output from the event's first application. This is part of
    /// replay history and must not be recomputed from later registration state.
    invalidations: Vec<InvalidationIdentity>,
}

/// Bounded, single-owner registration and replay state machine.
///
/// Callers must serialize access to this value. `register` installs the live
/// registration and computes replay under the same mutable borrow as
/// `apply_event`, which closes the evaluate-to-register blind spot. Runtime
/// integration can put this state machine behind a mutex or an actor mailbox.
/// Durable owner state/delivery, owner fencing, multi-owner activation
/// barriers, and catalog-version transitions intentionally remain outside this
/// single-owner primitive.
pub struct InvalidationOwnerRegistry {
    owner_id: InvalidationOwnerId,
    catalog_version: u64,
    cursor: InvalidationCursor,
    owner_sequence: u64,
    retained_from_exclusive: InvalidationCursor,
    retained_from_owner_sequence_exclusive: u64,
    max_replay_events: usize,
    events: BTreeMap<InvalidationEventId, RetainedInvalidationEvent>,
    registration_states: BTreeMap<InvalidationRegistrationId, InvalidationRegistrationState>,
}

impl InvalidationOwnerRegistry {
    pub fn new(
        owner_id: InvalidationOwnerId,
        catalog_version: u64,
        initial_cursor: InvalidationCursor,
        initial_owner_sequence: u64,
        max_replay_events: usize,
    ) -> Self {
        assert!(max_replay_events > 0, "replay journal must retain events");
        Self {
            owner_id,
            catalog_version,
            cursor: initial_cursor,
            owner_sequence: initial_owner_sequence,
            retained_from_exclusive: initial_cursor,
            retained_from_owner_sequence_exclusive: initial_owner_sequence,
            max_replay_events,
            events: BTreeMap::new(),
            registration_states: BTreeMap::new(),
        }
    }

    pub fn register(
        &mut self,
        registration: InvalidationRegistration,
    ) -> Result<RegistrationReplay, InvalidationRegistryError> {
        self.validate_protocol_and_catalog(
            registration.protocol_version,
            &registration.owner_id,
            registration.catalog_version,
        )?;
        if let Some(current) = self.registration_states.get(&registration.registration_id) {
            if registration.generation < current.generation() {
                return Err(InvalidationRegistryError::StaleRegistrationGeneration {
                    current: current.generation(),
                    received: registration.generation,
                });
            }
            if registration.generation == current.generation() {
                match current {
                    InvalidationRegistrationState::Active(current) if current == &registration => {
                    },
                    InvalidationRegistrationState::Active(_)
                    | InvalidationRegistrationState::Tombstone(_) => {
                        return Err(InvalidationRegistryError::RegistrationGenerationCollision {
                            generation: registration.generation,
                        });
                    },
                }
            }
        }
        if self.cursor < registration.evaluation_cursor {
            return Err(InvalidationRegistryError::OwnerBehind {
                owner_cursor: self.cursor,
                evaluation_cursor: registration.evaluation_cursor,
            });
        }
        if registration.evaluation_cursor < self.retained_from_exclusive {
            return Err(InvalidationRegistryError::ReplayGap {
                retained_from_exclusive: self.retained_from_exclusive,
                evaluation_cursor: registration.evaluation_cursor,
            });
        }

        let from_exclusive = registration.evaluation_cursor;
        let through_inclusive = self.cursor;
        let invalidations = self
            .events
            .values()
            .filter(|retained| {
                registration.evaluation_cursor < retained.event.commit_cursor
                    && retained.event.commit_cursor <= through_inclusive
                    && retained.event.matches(&registration)
            })
            .map(|retained| InvalidationIdentity {
                registration_id: registration.registration_id.clone(),
                registration_generation: registration.generation,
                event_id: retained.event.event_id.clone(),
                commit_cursor: retained.event.commit_cursor,
            })
            .collect();
        let registration_id = registration.registration_id.clone();
        self.registration_states.insert(
            registration_id,
            InvalidationRegistrationState::Active(registration),
        );
        Ok(RegistrationReplay {
            from_exclusive,
            through_inclusive,
            through_owner_sequence_inclusive: self.owner_sequence,
            invalidations,
        })
    }

    pub fn unregister(
        &mut self,
        unregistration: InvalidationUnregistration,
    ) -> Result<(), InvalidationRegistryError> {
        self.validate_protocol_and_catalog(
            unregistration.protocol_version,
            &unregistration.owner_id,
            unregistration.catalog_version,
        )?;
        if let Some(current) = self
            .registration_states
            .get(&unregistration.registration_id)
        {
            if unregistration.generation < current.generation() {
                return Err(InvalidationRegistryError::StaleRegistrationGeneration {
                    current: current.generation(),
                    received: unregistration.generation,
                });
            }
            if unregistration.generation == current.generation() {
                match current {
                    InvalidationRegistrationState::Active(_) => {},
                    InvalidationRegistrationState::Tombstone(current)
                        if current == &unregistration =>
                    {
                        return Ok(());
                    },
                    InvalidationRegistrationState::Tombstone(_) => {
                        return Err(InvalidationRegistryError::RegistrationGenerationCollision {
                            generation: unregistration.generation,
                        });
                    },
                }
            }
        }
        self.registration_states.insert(
            unregistration.registration_id.clone(),
            InvalidationRegistrationState::Tombstone(unregistration),
        );
        Ok(())
    }

    pub fn apply_event(
        &mut self,
        event: InvalidationEvent,
    ) -> Result<Vec<InvalidationIdentity>, InvalidationRegistryError> {
        self.validate_protocol_and_catalog(
            event.protocol_version,
            &event.event_id.owner_id,
            event.catalog_version,
        )?;
        if event.event_id.owner_sequence <= self.owner_sequence {
            return match self.events.get(&event.event_id) {
                Some(retained) if retained.event == event => Ok(retained.invalidations.clone()),
                Some(_) => Err(InvalidationRegistryError::EventIdentityCollision {
                    owner_sequence: event.event_id.owner_sequence,
                }),
                None => Err(InvalidationRegistryError::EventOutsideReplayWindow {
                    owner_sequence: event.event_id.owner_sequence,
                    retained_from_owner_sequence_exclusive: self
                        .retained_from_owner_sequence_exclusive,
                }),
            };
        }
        let expected_sequence = self.owner_sequence.checked_add(1).unwrap_or(u64::MAX);
        if event.event_id.owner_sequence != expected_sequence
            || event.predecessor_owner_sequence != self.owner_sequence
        {
            return Err(InvalidationRegistryError::OwnerSequenceGap {
                expected_sequence,
                received_sequence: event.event_id.owner_sequence,
                expected_predecessor: self.owner_sequence,
                received_predecessor: event.predecessor_owner_sequence,
            });
        }
        if event.commit_cursor <= self.cursor {
            return Err(InvalidationRegistryError::ReorderedCommitCursor {
                owner_cursor: self.cursor,
                event_cursor: event.commit_cursor,
            });
        }
        self.cursor = event.commit_cursor;
        self.owner_sequence = event.event_id.owner_sequence;
        let invalidations = self.invalidation_identities_for_event(&event);
        self.events.insert(
            event.event_id.clone(),
            RetainedInvalidationEvent {
                event,
                invalidations: invalidations.clone(),
            },
        );
        self.enforce_replay_bound();
        Ok(invalidations)
    }

    fn invalidation_identities_for_event(
        &self,
        event: &InvalidationEvent,
    ) -> Vec<InvalidationIdentity> {
        self.registration_states
            .values()
            .filter_map(InvalidationRegistrationState::active)
            .filter(|registration| {
                registration.evaluation_cursor < event.commit_cursor && event.matches(registration)
            })
            .map(|registration| InvalidationIdentity {
                registration_id: registration.registration_id.clone(),
                registration_generation: registration.generation,
                event_id: event.event_id.clone(),
                commit_cursor: event.commit_cursor,
            })
            .collect()
    }

    fn validate_protocol_and_catalog(
        &self,
        protocol_version: u16,
        owner_id: &InvalidationOwnerId,
        catalog_version: u64,
    ) -> Result<(), InvalidationRegistryError> {
        if protocol_version != INVALIDATION_OWNERSHIP_PROTOCOL_VERSION {
            return Err(InvalidationRegistryError::UnsupportedProtocol {
                received: protocol_version,
            });
        }
        if owner_id != &self.owner_id {
            return Err(InvalidationRegistryError::WrongOwner);
        }
        if catalog_version != self.catalog_version {
            return Err(InvalidationRegistryError::CatalogVersionMismatch {
                expected: self.catalog_version,
                received: catalog_version,
            });
        }
        Ok(())
    }

    fn enforce_replay_bound(&mut self) {
        while self.events.len() > self.max_replay_events {
            let oldest = self
                .events
                .keys()
                .min_by_key(|event_id| event_id.owner_sequence)
                .cloned()
                .expect("events exceeded a positive bound");
            let evicted = self
                .events
                .remove(&oldest)
                .expect("oldest event must still be retained");
            self.retained_from_exclusive = self
                .retained_from_exclusive
                .max(evicted.event.commit_cursor);
            self.retained_from_owner_sequence_exclusive = self
                .retained_from_owner_sequence_exclusive
                .max(oldest.owner_sequence);
        }
    }
}

/// Subscription-host deduplicator. Invalidation order is intentionally
/// irrelevant: each identity is an idempotent signal to rerun at a newer safe
/// snapshot.
pub struct InvalidationInbox {
    max_seen: usize,
    insertion_order: VecDeque<InvalidationIdentity>,
    seen: BTreeSet<InvalidationIdentity>,
}

impl InvalidationInbox {
    pub fn new(max_seen: usize) -> Self {
        assert!(
            max_seen > 0,
            "invalidation deduplication must retain identities"
        );
        Self {
            max_seen,
            insertion_order: VecDeque::new(),
            seen: BTreeSet::new(),
        }
    }

    /// Returns true only for the first delivery of this invalidation identity.
    /// Once an identity ages out, accepting it again can only cause a safe
    /// duplicate query rerun.
    pub fn accept(&mut self, identity: InvalidationIdentity) -> bool {
        if !self.seen.insert(identity.clone()) {
            return false;
        }
        self.insertion_order.push_back(identity);
        while self.insertion_order.len() > self.max_seen {
            let evicted = self
                .insertion_order
                .pop_front()
                .expect("deduplication queue exceeded a positive bound");
            self.seen.remove(&evicted);
        }
        true
    }
}

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
    use std::{
        collections::BTreeMap,
        time::Duration,
    };

    use common::{
        bootstrap_model::index::database_index::IndexedFields,
        interval::{
            BinaryKey,
            End,
            Interval,
            IntervalSet,
            StartIncluded,
        },
        types::TabletIndexName,
    };
    use maplit::btreeset;
    use value::{
        InternalId,
        TableName,
        TabletId,
    };

    use crate::{
        delta_interest::{
            dependencies_from_read_set,
            DeltaInterestTracker,
            InvalidationCursor,
            InvalidationDependency,
            InvalidationEvent,
            InvalidationEventId,
            InvalidationIdentity,
            InvalidationInbox,
            InvalidationIndex,
            InvalidationOwnerId,
            InvalidationOwnerRegistry,
            InvalidationRegistration,
            InvalidationRegistrationId,
            InvalidationRegistryError,
            InvalidationUnregistration,
            InvalidationWrite,
            INVALIDATION_OWNERSHIP_PROTOCOL_VERSION,
        },
        reads::{
            IndexReads,
            ReadSet,
        },
    };

    const CATALOG_VERSION: u64 = 7;

    fn owner(name: &str) -> InvalidationOwnerId {
        InvalidationOwnerId(name.to_string())
    }

    fn index(tablet_id: &str, descriptor: &str) -> InvalidationIndex {
        InvalidationIndex {
            tablet_id: tablet_id.to_string(),
            descriptor: descriptor.to_string(),
        }
    }

    fn registration(
        owner_id: InvalidationOwnerId,
        registration_id: &str,
        evaluation_cursor: u64,
        dependencies: impl IntoIterator<Item = InvalidationDependency>,
    ) -> InvalidationRegistration {
        InvalidationRegistration {
            protocol_version: INVALIDATION_OWNERSHIP_PROTOCOL_VERSION,
            registration_id: InvalidationRegistrationId(registration_id.to_string()),
            generation: 1,
            owner_id,
            catalog_version: CATALOG_VERSION,
            evaluation_cursor: InvalidationCursor(evaluation_cursor),
            dependencies: dependencies.into_iter().collect(),
        }
    }

    fn registration_at_generation(
        owner_id: InvalidationOwnerId,
        registration_id: &str,
        generation: u64,
        evaluation_cursor: u64,
        dependencies: impl IntoIterator<Item = InvalidationDependency>,
    ) -> InvalidationRegistration {
        let mut registration =
            registration(owner_id, registration_id, evaluation_cursor, dependencies);
        registration.generation = generation;
        registration
    }

    fn unregistration(
        owner_id: InvalidationOwnerId,
        registration_id: &str,
        generation: u64,
    ) -> InvalidationUnregistration {
        InvalidationUnregistration {
            protocol_version: INVALIDATION_OWNERSHIP_PROTOCOL_VERSION,
            registration_id: InvalidationRegistrationId(registration_id.to_string()),
            generation,
            owner_id,
            catalog_version: CATALOG_VERSION,
        }
    }

    fn event(
        owner_id: InvalidationOwnerId,
        cursor: u64,
        tablet_id: &str,
        descriptor: Option<&str>,
        key: Option<&[u8]>,
    ) -> InvalidationEvent {
        event_with_contract(
            owner_id,
            cursor,
            cursor,
            cursor.saturating_sub(1),
            tablet_id,
            descriptor,
            key,
            true,
        )
    }

    fn event_with_contract(
        owner_id: InvalidationOwnerId,
        cursor: u64,
        owner_sequence: u64,
        predecessor_owner_sequence: u64,
        tablet_id: &str,
        descriptor: Option<&str>,
        key: Option<&[u8]>,
        index_keys_complete: bool,
    ) -> InvalidationEvent {
        let mut index_keys = BTreeMap::new();
        if let Some(descriptor) = descriptor {
            index_keys.insert(
                descriptor.to_string(),
                key.into_iter().map(<[u8]>::to_vec).collect(),
            );
        }
        InvalidationEvent {
            protocol_version: INVALIDATION_OWNERSHIP_PROTOCOL_VERSION,
            event_id: InvalidationEventId {
                owner_id,
                owner_sequence,
            },
            predecessor_owner_sequence,
            commit_cursor: InvalidationCursor(cursor),
            catalog_version: CATALOG_VERSION,
            writes: vec![InvalidationWrite {
                tablet_id: tablet_id.to_string(),
                index_keys_complete,
                index_keys,
            }],
        }
    }

    #[test]
    fn read_set_conversion_preserves_point_range_and_scan_dependencies() {
        let point_tablet = TabletId(InternalId([1; 16]));
        let range_tablet = TabletId(InternalId([2; 16]));
        let scan_tablet = TabletId(InternalId([3; 16]));
        let point_index = TabletIndexName::by_id(point_tablet);
        let range_index = TabletIndexName::by_creation_time(range_tablet);
        let scan_index = TabletIndexName::by_creation_time(scan_tablet);

        let mut point_intervals = IntervalSet::new();
        point_intervals.add(Interval::singleton(BinaryKey::from(vec![1, 2])));
        let mut range_intervals = IntervalSet::new();
        range_intervals.add(Interval {
            start: StartIncluded(BinaryKey::from(vec![3])),
            end: End::Excluded(BinaryKey::from(vec![8])),
        });
        let mut scan_intervals = IntervalSet::new();
        scan_intervals.add(Interval::all());
        let reads = ReadSet::new(
            BTreeMap::from([
                (
                    point_index,
                    IndexReads {
                        fields: IndexedFields::by_id(),
                        intervals: point_intervals,
                        stack_traces: None,
                    },
                ),
                (
                    range_index,
                    IndexReads {
                        fields: IndexedFields::by_id(),
                        intervals: range_intervals,
                        stack_traces: None,
                    },
                ),
                (
                    scan_index,
                    IndexReads {
                        fields: IndexedFields::by_id(),
                        intervals: scan_intervals,
                        stack_traces: None,
                    },
                ),
            ]),
            BTreeMap::new(),
        );

        let dependencies = dependencies_from_read_set(&reads);
        assert!(dependencies.iter().any(|dependency| matches!(
            dependency,
            InvalidationDependency::Point { key, .. } if key == &[1, 2]
        )));
        assert!(dependencies.iter().any(|dependency| matches!(
            dependency,
            InvalidationDependency::IndexRange {
                start_inclusive,
                end_exclusive: Some(end_exclusive),
                ..
            } if start_inclusive == &[3] && end_exclusive == &[8]
        )));
        assert!(dependencies.contains(&InvalidationDependency::TableScan {
            tablet_id: scan_tablet.to_string(),
        }));
    }

    #[test]
    fn point_dependencies_match_only_the_point() {
        let owner_id = owner("owner-a");
        let mut registry = InvalidationOwnerRegistry::new(
            owner_id.clone(),
            CATALOG_VERSION,
            InvalidationCursor(10),
            10,
            16,
        );
        registry
            .register(registration(
                owner_id.clone(),
                "subscription-a",
                10,
                [InvalidationDependency::Point {
                    index: index("tasks", "by_id"),
                    key: b"task-1".to_vec(),
                }],
            ))
            .unwrap();

        assert!(registry
            .apply_event(event(
                owner_id.clone(),
                11,
                "tasks",
                Some("by_id"),
                Some(b"task-2"),
            ))
            .unwrap()
            .is_empty());
        assert_eq!(
            registry
                .apply_event(event(owner_id, 12, "tasks", Some("by_id"), Some(b"task-1"),))
                .unwrap()
                .len(),
            1
        );
    }

    #[test]
    fn range_dependencies_match_index_keys_and_missing_keys_conservatively() {
        let owner_id = owner("owner-a");
        let mut registry = InvalidationOwnerRegistry::new(
            owner_id.clone(),
            CATALOG_VERSION,
            InvalidationCursor(20),
            20,
            16,
        );
        registry
            .register(registration(
                owner_id.clone(),
                "subscription-a",
                20,
                [InvalidationDependency::IndexRange {
                    index: index("tasks", "by_status"),
                    start_inclusive: b"open".to_vec(),
                    end_exclusive: Some(b"pending".to_vec()),
                }],
            ))
            .unwrap();

        assert!(registry
            .apply_event(event(
                owner_id.clone(),
                21,
                "tasks",
                Some("by_status"),
                Some(b"closed"),
            ))
            .unwrap()
            .is_empty());
        assert_eq!(
            registry
                .apply_event(event(
                    owner_id.clone(),
                    22,
                    "tasks",
                    Some("by_status"),
                    Some(b"open"),
                ))
                .unwrap()
                .len(),
            1
        );
        assert_eq!(
            registry
                .apply_event(event(owner_id, 23, "tasks", None, None))
                .unwrap()
                .len(),
            1,
            "missing index keys must cause a safe false positive"
        );
    }

    #[test]
    fn uncertified_index_keys_invalidate_even_when_known_keys_do_not_match() {
        let owner_id = owner("owner-a");
        let mut registry = InvalidationOwnerRegistry::new(
            owner_id.clone(),
            CATALOG_VERSION,
            InvalidationCursor(24),
            24,
            16,
        );
        registry
            .register(registration(
                owner_id.clone(),
                "subscription-a",
                24,
                [InvalidationDependency::Point {
                    index: index("tasks", "by_id"),
                    key: b"task-1".to_vec(),
                }],
            ))
            .unwrap();

        let incomplete = event_with_contract(
            owner_id,
            25,
            25,
            24,
            "tasks",
            Some("by_id"),
            Some(b"different-task"),
            false,
        );
        assert_eq!(registry.apply_event(incomplete).unwrap().len(), 1);
    }

    #[test]
    fn table_scan_matches_every_write_to_the_table() {
        let owner_id = owner("owner-a");
        let mut registry = InvalidationOwnerRegistry::new(
            owner_id.clone(),
            CATALOG_VERSION,
            InvalidationCursor(30),
            30,
            16,
        );
        registry
            .register(registration(
                owner_id.clone(),
                "subscription-a",
                30,
                [InvalidationDependency::TableScan {
                    tablet_id: "messages".to_string(),
                }],
            ))
            .unwrap();

        assert!(registry
            .apply_event(event(owner_id.clone(), 31, "tasks", None, None))
            .unwrap()
            .is_empty());
        assert_eq!(
            registry
                .apply_event(event(owner_id, 32, "messages", None, None))
                .unwrap()
                .len(),
            1
        );
    }

    #[test]
    fn one_subscription_can_register_dependencies_with_multiple_owners() {
        let owner_a = owner("owner-a");
        let owner_b = owner("owner-b");
        let mut registry_a = InvalidationOwnerRegistry::new(
            owner_a.clone(),
            CATALOG_VERSION,
            InvalidationCursor(40),
            40,
            16,
        );
        let mut registry_b = InvalidationOwnerRegistry::new(
            owner_b.clone(),
            CATALOG_VERSION,
            InvalidationCursor(40),
            40,
            16,
        );
        registry_a
            .register(registration(
                owner_a.clone(),
                "subscription-a",
                40,
                [InvalidationDependency::TableScan {
                    tablet_id: "users".to_string(),
                }],
            ))
            .unwrap();
        registry_b
            .register(registration(
                owner_b.clone(),
                "subscription-a",
                40,
                [InvalidationDependency::TableScan {
                    tablet_id: "tasks".to_string(),
                }],
            ))
            .unwrap();

        let from_a = registry_a
            .apply_event(event(owner_a, 41, "users", None, None))
            .unwrap();
        let from_b = registry_b
            .apply_event(event(owner_b, 41, "tasks", None, None))
            .unwrap();
        assert_eq!(from_a.len(), 1);
        assert_eq!(from_b.len(), 1);
        assert_ne!(from_a[0], from_b[0]);
        assert_eq!(
            from_a[0].registration_id,
            InvalidationRegistrationId("subscription-a".to_string())
        );
        assert_eq!(from_a[0].registration_id, from_b[0].registration_id);
    }

    #[test]
    fn invalidation_inbox_deduplicates_duplicate_and_reordered_deliveries() {
        let registration_id = InvalidationRegistrationId("subscription-a".to_string());
        let identity_at = |cursor| InvalidationIdentity {
            registration_id: registration_id.clone(),
            registration_generation: 1,
            event_id: InvalidationEventId {
                owner_id: owner("owner-a"),
                owner_sequence: cursor,
            },
            commit_cursor: InvalidationCursor(cursor),
        };
        let older = identity_at(50);
        let newer = identity_at(51);
        let newest = identity_at(52);
        let mut inbox = InvalidationInbox::new(2);

        assert!(inbox.accept(newer.clone()));
        assert!(inbox.accept(older.clone()), "delivery order is irrelevant");
        assert!(!inbox.accept(newer));
        assert!(!inbox.accept(older.clone()));
        assert!(inbox.accept(newest));
        assert!(
            inbox.accept(identity_at(51)),
            "an evicted identity may cause a safe duplicate rerun"
        );
    }

    #[test]
    fn retained_event_replay_reemits_identities_and_rejects_payload_mismatch() {
        let owner_id = owner("owner-a");
        let dependency = InvalidationDependency::TableScan {
            tablet_id: "tasks".to_string(),
        };
        let mut registry = InvalidationOwnerRegistry::new(
            owner_id.clone(),
            CATALOG_VERSION,
            InvalidationCursor(55),
            55,
            16,
        );
        registry
            .register(registration(
                owner_id.clone(),
                "subscription-a",
                55,
                [dependency.clone()],
            ))
            .unwrap();
        let committed = event(owner_id.clone(), 56, "tasks", None, None);
        let first_delivery = registry.apply_event(committed.clone()).unwrap();
        assert_eq!(first_delivery.len(), 1);
        let first_delivery_bytes = serde_json::to_vec(&first_delivery).unwrap();

        registry
            .unregister(unregistration(owner_id.clone(), "subscription-a", 1))
            .unwrap();
        registry
            .register(registration_at_generation(
                owner_id.clone(),
                "subscription-a",
                2,
                56,
                [dependency.clone()],
            ))
            .unwrap();
        registry
            .register(registration(
                owner_id.clone(),
                "subscription-b",
                56,
                [dependency],
            ))
            .unwrap();
        let current_delivery = registry
            .apply_event(event(owner_id, 57, "tasks", None, None))
            .unwrap();
        assert_eq!(current_delivery.len(), 2);

        let repeated_delivery = registry.apply_event(committed.clone()).unwrap();
        assert_eq!(
            repeated_delivery, first_delivery,
            "replay output is journaled independently of current registrations"
        );
        assert_eq!(
            serde_json::to_vec(&repeated_delivery).unwrap(),
            first_delivery_bytes,
            "replay output preserves deterministic wire ordering"
        );
        let mut conflicting = committed;
        conflicting.writes[0].tablet_id = "messages".to_string();
        assert!(matches!(
            registry.apply_event(conflicting),
            Err(InvalidationRegistryError::EventIdentityCollision { owner_sequence: 56 })
        ));
    }

    #[test]
    fn evicted_event_identity_fails_closed_for_exact_and_conflicting_payloads() {
        let owner_id = owner("owner-a");
        let mut registry = InvalidationOwnerRegistry::new(
            owner_id.clone(),
            CATALOG_VERSION,
            InvalidationCursor(55),
            55,
            1,
        );
        let evicted = event(owner_id.clone(), 56, "tasks", None, None);
        registry.apply_event(evicted.clone()).unwrap();
        registry
            .apply_event(event(owner_id, 57, "messages", None, None))
            .unwrap();

        assert!(matches!(
            registry.apply_event(evicted.clone()),
            Err(InvalidationRegistryError::EventOutsideReplayWindow {
                owner_sequence: 56,
                retained_from_owner_sequence_exclusive: 56,
            })
        ));
        let mut conflicting = evicted;
        conflicting.writes[0].tablet_id = "users".to_string();
        assert!(matches!(
            registry.apply_event(conflicting),
            Err(InvalidationRegistryError::EventOutsideReplayWindow {
                owner_sequence: 56,
                retained_from_owner_sequence_exclusive: 56,
            })
        ));
    }

    #[test]
    fn owner_sequence_allows_global_cursor_jumps() {
        let owner_id = owner("owner-a");
        let mut registry = InvalidationOwnerRegistry::new(
            owner_id.clone(),
            CATALOG_VERSION,
            InvalidationCursor(100),
            7,
            16,
        );
        let next = event_with_contract(owner_id.clone(), 1_000, 8, 7, "tasks", None, None, false);
        assert!(registry.apply_event(next).unwrap().is_empty());
    }

    #[test]
    fn owner_sequence_rejects_wrong_predecessor_for_the_next_sequence() {
        let owner_id = owner("owner-a");
        let mut registry = InvalidationOwnerRegistry::new(
            owner_id.clone(),
            CATALOG_VERSION,
            InvalidationCursor(100),
            7,
            16,
        );
        let wrong_predecessor =
            event_with_contract(owner_id, 101, 8, 6, "tasks", None, None, false);
        assert!(matches!(
            registry.apply_event(wrong_predecessor),
            Err(InvalidationRegistryError::OwnerSequenceGap {
                expected_sequence: 8,
                received_sequence: 8,
                expected_predecessor: 7,
                received_predecessor: 6,
            })
        ));
    }

    #[test]
    fn owner_sequence_rejects_non_contiguous_sequence_with_current_predecessor() {
        let owner_id = owner("owner-a");
        let mut registry = InvalidationOwnerRegistry::new(
            owner_id.clone(),
            CATALOG_VERSION,
            InvalidationCursor(100),
            7,
            16,
        );
        let skipped_sequence = event_with_contract(owner_id, 101, 9, 7, "tasks", None, None, false);
        assert!(matches!(
            registry.apply_event(skipped_sequence),
            Err(InvalidationRegistryError::OwnerSequenceGap {
                expected_sequence: 8,
                received_sequence: 9,
                expected_predecessor: 7,
                received_predecessor: 7,
            })
        ));
    }

    #[test]
    fn registration_replays_writes_between_evaluation_and_installation() {
        let owner_id = owner("owner-a");
        let mut registry = InvalidationOwnerRegistry::new(
            owner_id.clone(),
            CATALOG_VERSION,
            InvalidationCursor(60),
            60,
            16,
        );
        registry
            .apply_event(event(owner_id.clone(), 61, "tasks", None, None))
            .unwrap();
        registry
            .apply_event(event(owner_id.clone(), 62, "messages", None, None))
            .unwrap();

        let replay = registry
            .register(registration(
                owner_id.clone(),
                "subscription-a",
                60,
                [InvalidationDependency::TableScan {
                    tablet_id: "tasks".to_string(),
                }],
            ))
            .unwrap();
        assert_eq!(replay.from_exclusive, InvalidationCursor(60));
        assert_eq!(replay.through_inclusive, InvalidationCursor(62));
        assert_eq!(replay.invalidations.len(), 1);
        assert_eq!(
            replay.invalidations[0].commit_cursor,
            InvalidationCursor(61)
        );

        assert_eq!(
            registry
                .apply_event(event(owner_id, 63, "tasks", None, None))
                .unwrap()
                .len(),
            1,
            "writes after the registration cursor use live matching"
        );
    }

    #[test]
    fn registration_fails_closed_when_owner_is_behind_or_replay_has_a_gap() {
        let owner_id = owner("owner-a");
        let mut behind = InvalidationOwnerRegistry::new(
            owner_id.clone(),
            CATALOG_VERSION,
            InvalidationCursor(69),
            69,
            16,
        );
        assert!(matches!(
            behind.register(registration(owner_id.clone(), "behind", 70, [])),
            Err(InvalidationRegistryError::OwnerBehind { .. })
        ));

        let mut truncated = InvalidationOwnerRegistry::new(
            owner_id.clone(),
            CATALOG_VERSION,
            InvalidationCursor(70),
            70,
            1,
        );
        truncated
            .apply_event(event(owner_id.clone(), 71, "tasks", None, None))
            .unwrap();
        truncated
            .apply_event(event(owner_id.clone(), 72, "tasks", None, None))
            .unwrap();
        assert!(matches!(
            truncated.register(registration(owner_id, "gap", 70, [])),
            Err(InvalidationRegistryError::ReplayGap { .. })
        ));
    }

    #[test]
    fn stale_registration_generation_cannot_replace_a_newer_incarnation() {
        let owner_id = owner("owner-a");
        let dependency = InvalidationDependency::TableScan {
            tablet_id: "tasks".to_string(),
        };
        let mut registry = InvalidationOwnerRegistry::new(
            owner_id.clone(),
            CATALOG_VERSION,
            InvalidationCursor(75),
            75,
            16,
        );
        registry
            .register(registration_at_generation(
                owner_id.clone(),
                "subscription-a",
                2,
                75,
                [dependency.clone()],
            ))
            .unwrap();
        assert!(matches!(
            registry.register(registration_at_generation(
                owner_id.clone(),
                "subscription-a",
                1,
                75,
                [dependency.clone()],
            )),
            Err(InvalidationRegistryError::StaleRegistrationGeneration {
                current: 2,
                received: 1,
            })
        ));
        assert!(matches!(
            registry.register(registration_at_generation(
                owner_id.clone(),
                "subscription-a",
                2,
                75,
                [InvalidationDependency::TableScan {
                    tablet_id: "messages".to_string(),
                }],
            )),
            Err(InvalidationRegistryError::RegistrationGenerationCollision { generation: 2 })
        ));
        registry
            .register(registration_at_generation(
                owner_id.clone(),
                "subscription-a",
                3,
                75,
                [dependency],
            ))
            .unwrap();
        let invalidations = registry
            .apply_event(event(owner_id, 76, "tasks", None, None))
            .unwrap();
        assert_eq!(invalidations.len(), 1);
        assert_eq!(invalidations[0].registration_generation, 3);
    }

    #[test]
    fn stale_unregistration_cannot_remove_or_resurrect_a_newer_registration() {
        let owner_id = owner("owner-a");
        let dependency = InvalidationDependency::TableScan {
            tablet_id: "tasks".to_string(),
        };
        let mut registry = InvalidationOwnerRegistry::new(
            owner_id.clone(),
            CATALOG_VERSION,
            InvalidationCursor(80),
            80,
            16,
        );
        registry
            .register(registration_at_generation(
                owner_id.clone(),
                "subscription-a",
                2,
                80,
                [dependency.clone()],
            ))
            .unwrap();

        assert!(matches!(
            registry.unregister(unregistration(owner_id.clone(), "subscription-a", 1,)),
            Err(InvalidationRegistryError::StaleRegistrationGeneration {
                current: 2,
                received: 1,
            })
        ));
        let still_active = registry
            .apply_event(event(owner_id.clone(), 81, "tasks", None, None))
            .unwrap();
        assert_eq!(still_active.len(), 1);
        assert_eq!(still_active[0].registration_generation, 2);

        let mut wrong_owner = unregistration(owner("owner-b"), "subscription-a", 2);
        assert!(matches!(
            registry.unregister(wrong_owner.clone()),
            Err(InvalidationRegistryError::WrongOwner)
        ));
        wrong_owner.owner_id = owner_id.clone();
        wrong_owner.catalog_version += 1;
        assert!(matches!(
            registry.unregister(wrong_owner),
            Err(InvalidationRegistryError::CatalogVersionMismatch { .. })
        ));

        let tombstone = unregistration(owner_id.clone(), "subscription-a", 2);
        registry.unregister(tombstone.clone()).unwrap();
        registry
            .unregister(tombstone)
            .expect("an exact tombstone retry is idempotent");
        assert!(
            registry
                .apply_event(event(owner_id.clone(), 82, "tasks", None, None))
                .unwrap()
                .is_empty(),
            "the tombstone removes the active generation"
        );
        assert!(matches!(
            registry.register(registration_at_generation(
                owner_id.clone(),
                "subscription-a",
                1,
                82,
                [dependency.clone()],
            )),
            Err(InvalidationRegistryError::StaleRegistrationGeneration {
                current: 2,
                received: 1,
            })
        ));
        assert!(matches!(
            registry.register(registration_at_generation(
                owner_id.clone(),
                "subscription-a",
                2,
                82,
                [dependency.clone()],
            )),
            Err(InvalidationRegistryError::RegistrationGenerationCollision { generation: 2 })
        ));
        registry
            .register(registration_at_generation(
                owner_id,
                "subscription-a",
                3,
                82,
                [dependency],
            ))
            .unwrap();
    }

    #[test]
    fn registration_protocol_is_versioned_and_serializable() {
        let registration = registration(
            owner("owner-a"),
            "subscription-a",
            80,
            [InvalidationDependency::TableScan {
                tablet_id: "tasks".to_string(),
            }],
        );
        let json = serde_json::to_string(&registration).unwrap();
        let decoded: InvalidationRegistration = serde_json::from_str(&json).unwrap();
        assert_eq!(decoded, registration);

        let unregistration = unregistration(owner("owner-a"), "subscription-a", 1);
        let json = serde_json::to_string(&unregistration).unwrap();
        let decoded: InvalidationUnregistration = serde_json::from_str(&json).unwrap();
        assert_eq!(decoded, unregistration);

        let mut wrong_version = registration;
        wrong_version.protocol_version += 1;
        let mut registry = InvalidationOwnerRegistry::new(
            owner("owner-a"),
            CATALOG_VERSION,
            InvalidationCursor(80),
            80,
            16,
        );
        assert!(matches!(
            registry.register(wrong_version),
            Err(InvalidationRegistryError::UnsupportedProtocol { .. })
        ));
    }

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
