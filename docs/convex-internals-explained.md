# Convex Internals: How the Backend Actually Works

A practical explanation of the five core components that make Convex tick — and why each one is a barrier to horizontal scaling.

---

## Table of Contents

1. [The Committer](#1-the-committer)
2. [In-Memory Snapshots (Snapshot Manager)](#2-in-memory-snapshots-snapshot-manager)
3. [The Write Log](#3-the-write-log)
4. [Subscriptions (Real-Time System)](#4-subscriptions-real-time-system)
5. [OCC (Optimistic Concurrency Control)](#5-occ-optimistic-concurrency-control)
6. [How They All Connect](#6-how-they-all-connect)
7. [Why This Architecture Can't Just Be "Deployed Twice"](#7-why-this-architecture-cant-just-be-deployed-twice)

---

## 1. The Committer

### What It Is

The Committer is a single background task (an async loop running on one thread) that is the **only thing allowed to write to the database**. Every mutation in your entire Convex deployment — whether it's a user signing up, a message being sent, or a file being deleted — gets sent to the Committer as a message on a channel.

The Committer processes these messages **one at a time, sequentially**. For each one, it:

1. Validates the transaction (checks for conflicts — more on this in the OCC section)
2. Assigns a **monotonically increasing timestamp** (101, 102, 103... never out of order, never duplicated)
3. Writes the changes to the persistence layer (SQLite, Postgres, or MySQL)
4. Publishes the changes to the write log and snapshot manager

### Why It's Single-Threaded

Serializability. If two mutations both try to modify the same document, the Committer decides the order. There's no ambiguity, no race condition, no need for distributed locking. Transaction A gets timestamp 100, Transaction B gets timestamp 101. Done.

This is the same pattern as Redis (single-threaded command processing) and SQLite (single-writer). It's the simplest way to guarantee that every transaction sees a consistent view of the database.

### The Code

In `crates/database/src/committer.rs`, the Committer is structured as:

```
CommitterClient (what the rest of the system talks to)
    │
    │  sends CommitterMessage via mpsc channel
    ▼
Committer::go() — an async loop that:
    - recv() a message
    - validates the transaction
    - writes to persistence
    - publishes to write log
    - updates snapshot manager
    - sends back the commit timestamp via oneshot channel
```

The `CommitterMessage` enum has variants like:
- `Commit { transaction, result_sender, ... }` — the main one
- `FinishTextAndVectorBootstrap { ... }` — index initialization
- `LoadIndexesIntoMemory { ... }` — loading indexes at startup

### Why It Blocks Horizontal Scaling

Two nodes = two Committers = two independent timestamp sequences. Node A assigns timestamp 100 to a write on document X, Node B assigns timestamp 100 to a different write on document X. Both succeed. The database is now inconsistent — which write "won"? Nobody knows.

---

## 2. In-Memory Snapshots (Snapshot Manager)

### What It Is

The Snapshot Manager maintains a sliding window of **database snapshots at different points in time**, all held in RAM. Each snapshot contains:

| Component | What It Holds |
|-----------|--------------|
| `TableRegistry` | Every table's name, ID, document count, total size, and shape (schema) |
| `IndexRegistry` | Every index's name, ID, fields, and backfill state |
| `BackendInMemoryIndexes` | Actual index data for system tables, text search, and vector search |
| `SchemaRegistry` | Schema validation rules |
| `ComponentRegistry` | Component tree metadata |

These snapshots use **persistent data structures** (`imbl::OrdMap`) — immutable trees with structural sharing. When a new commit happens, Convex doesn't copy the entire snapshot. It creates a new version that shares most of its structure with the previous one, only allocating memory for the parts that changed. This is the same technique used by Git (content-addressed trees) and Clojure's data structures.

### Why It Lives in RAM

Performance. When a query runs, it needs to know things like "what indexes exist on the messages table?" and "what's the schema for the users table?". Reading this from disk on every query would add milliseconds of latency. Keeping it in RAM means these lookups are nanosecond-scale.

The persistence layer (SQLite/Postgres/MySQL) stores the same data durably on disk, but the Snapshot Manager is what transactions actually read from during execution. The disk copy is for crash recovery and bootstrap — when the backend starts up, it loads from persistence into the Snapshot Manager.

### Multi-Version Concurrency

The "sliding window" part is important. The Snapshot Manager doesn't just hold the latest state — it holds snapshots at multiple timestamps within a configurable window (`MAX_TRANSACTION_WINDOW`). This allows multiple transactions to be in-flight simultaneously, each reading from the snapshot at their own start timestamp, without blocking each other.

```
Snapshot at ts=98:  { messages: 500 docs, users: 100 docs }
Snapshot at ts=99:  { messages: 501 docs, users: 100 docs }  ← TX-A reads here
Snapshot at ts=100: { messages: 501 docs, users: 101 docs }  ← TX-B reads here
Snapshot at ts=101: { messages: 502 docs, users: 101 docs }  ← latest
```

Transaction A and Transaction B both execute concurrently, reading from different consistent snapshots. Neither blocks the other. Conflicts are only detected at commit time (see OCC section).

### Why It Blocks Horizontal Scaling

The Snapshot Manager is the authoritative state of the database. If Node A commits a new document, Node A's Snapshot Manager updates instantly. Node B's Snapshot Manager knows nothing about it. Node B is now serving stale reads.

You can't solve this by "just querying the disk database" because:
1. The persistence layer may not have flushed yet
2. In-memory indexes (text search, vector search) have no disk equivalent in real-time
3. The performance penalty would negate Convex's speed advantage

---

## 3. The Write Log

### What It Is

The Write Log is a short-lived, in-memory, append-only sequence of recent writes. Every time the Committer successfully commits a transaction, it appends an entry to the write log:

```
Entry: (timestamp=100, writes=[{doc_id=X, old=..., new=...}], source=mutation("sendMessage"))
Entry: (timestamp=101, writes=[{doc_id=Y, old=..., new=...}], source=mutation("updateUser"))
Entry: (timestamp=102, writes=[{doc_id=X, old=..., new=...}], source=mutation("editMessage"))
```

It's not a permanent log — entries are evicted after a retention window (configured by `WRITE_LOG_MAX_RETENTION_SECS` and `WRITE_LOG_SOFT_MAX_SIZE_BYTES`). It only needs to hold enough history to cover the transaction window and subscription invalidation.

### What It's Used For

Two things:

**1. OCC conflict detection.** When a transaction wants to commit, the Committer asks the write log: "Between timestamp 95 (when this transaction started) and now, did anyone write to any of the documents this transaction read?" If yes → conflict → reject and retry.

**2. Subscription invalidation.** Subscription workers continuously tail the write log, checking each new write against every active subscription's read set. If a write overlaps with a subscription's read set, that subscription is invalidated and the query is re-executed.

### The Three Types

The write log is split into three cooperating objects:

| Type | Who Uses It | Access |
|------|------------|--------|
| `LogWriter` | The Committer | Append-only. Writes new entries after each commit. Also used for conflict checking (`is_stale()`). |
| `LogReader` | Subscription workers, Database | Read-only. Tails the log via `wait_for_higher_ts()` and iterates via `for_each()`. |
| `LogOwner` | Database initialization | Lifecycle management. |

All three are backed by the same `Arc<Mutex<WriteLogManager>>` — a shared, lock-protected, in-memory data structure.

### Why It Blocks Horizontal Scaling

The write log is local to one process. Node A's LogWriter appends entries that Node A's LogReader can see. Node B has its own separate write log. This means:

- Node B's OCC validation can't detect conflicts caused by Node A's commits
- Node B's subscription workers can't invalidate subscriptions affected by Node A's writes

---

## 4. Subscriptions (Real-Time System)

### What It Is

Convex's real-time system. When your frontend uses `useQuery(api.messages.list, { channel: "general" })`, here's the lifecycle:

1. **Initial execution:** The backend runs the query function. During execution, it tracks every document and index range the function reads — this becomes the **read set**.

2. **Token creation:** The results + read set are bundled into a `Token`. The token represents "these results are valid as of timestamp X, and they depend on these specific documents/index ranges."

3. **Subscription registration:** The `Token` is passed to `SubscriptionsClient::subscribe()`, which registers it with one of the subscription workers (round-robin load balanced across `NUM_SUBSCRIPTION_MANAGERS` workers).

4. **Continuous monitoring:** Each subscription worker runs a loop:
   ```
   loop {
       new_ts = log.wait_for_higher_ts(current_ts)  // blocks until new writes appear
       for each write between current_ts and new_ts:
           for each subscription:
               if write overlaps subscription's read set:
                   invalidate(subscription)  // triggers re-execution + push to client
               else:
                   advance(subscription)     // subscription is still valid, move forward
   }
   ```

5. **Client notification:** When a subscription is invalidated, the backend re-runs the query, diffs the result, and pushes the update over the WebSocket.

### How Invalidation Works

The subscription doesn't just track document IDs — it tracks **index ranges**. If your query was "all messages where channel == 'general'", the read set includes the index range `[channel='general', -∞] to [channel='general', +∞]`. Any write that falls within that range invalidates the subscription, even if it's a new document that didn't exist when the query ran. This prevents phantom reads.

### Why It Blocks Horizontal Scaling

The subscription workers read from the local `LogReader`. If a commit happens on Node B that inserts a new message into channel "general", Node A's subscription workers never see that write. The client connected to Node A never gets the update.

Additionally, the subscription state itself (the mapping of tokens to read sets) lives in the memory of the node the client is connected to. There's no mechanism to transfer or replicate this state across nodes.

---

## 5. OCC (Optimistic Concurrency Control)

### What It Is

OCC is Convex's strategy for handling concurrent transactions without locks. The philosophy is:

- **Optimistic:** Assume there won't be a conflict. Let the transaction execute freely without acquiring any locks.
- **Validate at commit time:** Right before committing, check whether the data the transaction read has been modified by someone else since the transaction started.
- **Retry on conflict:** If validation fails, throw away the transaction's writes and re-execute the function from scratch with fresh data.

### The Flow

```
1. Begin transaction at ts=100
   → Take a snapshot of the database at ts=100
   → Record every read into a "read set"

2. Execute your mutation function
   → Reads: user.balance = $600 (recorded in read set)
   → Decides: set user.balance = $100
   → Writes are buffered locally, not yet committed

3. Send to Committer for validation
   → Committer checks write log: "Between ts=100 and now (ts=105),
     did anyone modify user.balance?"
   
4a. No conflict:
   → Assign ts=106
   → Write to persistence
   → Append to write log
   → Update snapshot manager
   → Return success

4b. Conflict detected (someone wrote to user.balance at ts=103):
   → Reject transaction
   → Convex automatically retries from step 1, this time reading at ts=105
   → Up to MAX_OCC_FAILURES retries (currently 3)
```

### Why "Optimistic" vs "Pessimistic"

**Pessimistic (what Postgres does with `SELECT ... FOR UPDATE`):**
- Acquires a lock on the row before reading
- Other transactions block until the lock is released
- Safe but slow under contention — transactions wait in line

**Optimistic (what Convex does):**
- No locks acquired during execution
- Multiple transactions can read the same data simultaneously
- Conflicts detected only at commit time
- Fast when contention is low (most of the time)
- Retries are needed when contention is high (rare)

Convex chose OCC because their transaction model — short-lived JavaScript/TypeScript functions that execute in <1 second — is well-suited to it. Functions are cheap to retry, and most transactions don't conflict.

### Why It Blocks Horizontal Scaling

The validation step (`is_stale()`) scans the local write log. This only works if **all** writes go through the same write log. With multiple nodes, each with its own write log, a write on Node B that conflicts with a transaction on Node A goes undetected. Node A's Committer approves the transaction because its write log shows no conflict. Serializability is now broken.

---

## 6. How They All Connect

Here's the full data flow for a single mutation, showing how all five components interact:

```
Client calls mutation("sendMessage", { channel: "general", text: "hello" })
    │
    ▼
Funrun (or in-process runner) executes the JavaScript function
    │
    │  1. Begins transaction → gets snapshot at ts=100 from SnapshotManager
    │  2. Reads channel doc (recorded in read set)
    │  3. Creates new message doc (buffered as pending write)
    │  4. Returns FunctionFinalTransaction to backend
    │
    ▼
Database.commit(transaction)
    │
    │  Sends CommitterMessage::Commit to Committer via mpsc channel
    │
    ▼
Committer receives the message
    │
    │  5. OCC check: scans WriteLog via is_stale()
    │     → "Has anyone written to the channel doc since ts=100?" → No
    │  6. Assigns timestamp ts=106
    │  7. Writes to Persistence (SQLite/Postgres/MySQL)
    │  8. Appends to WriteLog: (ts=106, [{new message doc}])
    │  9. Updates SnapshotManager: push new snapshot at ts=106
    │
    ▼
Subscription workers (already tailing the WriteLog)
    │
    │  10. log.wait_for_higher_ts(105) returns 106
    │  11. Iterates write at ts=106
    │  12. Checks: does this write overlap any subscription's read set?
    │      → Yes: Client Y subscribed to "messages where channel='general'"
    │  13. Invalidates Client Y's subscription
    │  14. Re-runs the query → gets updated message list
    │  15. Pushes result to Client Y over WebSocket
    │
    ▼
Client Y's UI updates in real-time
```

Every single step happens **within one process on one machine**. That's why it's fast (no network hops between components), and why it can't scale horizontally (no way to distribute these steps across machines without adding network hops and coordination protocols).

---

## 7. Why This Architecture Can't Just Be "Deployed Twice"

With a typical stateless web app (Express, Django, FastAPI), you can run 10 instances behind a load balancer because:
- No instance holds state — they all read from and write to an external database
- The external database (Postgres, MySQL) handles concurrency, ordering, and consistency
- Any request can go to any instance

Convex is different because **the backend IS the database**. It's not a stateless app that talks to a database — it's a stateful system that holds the live database in its own memory. Deploying it twice creates two independent databases that don't know about each other:

| What You'd Expect | What Actually Happens |
|---|---|
| Two nodes share one database | Two nodes each have their own in-memory database |
| A write on Node A is visible on Node B | Node B's snapshot doesn't update |
| A subscription on Node A sees writes from Node B | Node A's write log doesn't contain Node B's writes |
| Conflicts between Node A and Node B are detected | Each node only checks its own write log |

The persistence layer (SQLite/Postgres) is shared, but it's not the source of truth during operation — the in-memory Snapshot Manager is. Two nodes writing to the same Postgres database would corrupt it because their timestamp sequences would collide and their OCC validation wouldn't catch cross-node conflicts.

This is why the [horizontal scaling proposal](./horizontal-scaling-proposal.md) requires replacing the in-memory write log with a distributed log, adding a global timestamp oracle, and implementing cross-node subscription fanout. These are the minimum changes needed to make multiple Convex nodes behave as a single coherent system.
