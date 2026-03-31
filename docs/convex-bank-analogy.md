# Think of Convex as a Bank With One Teller

A practical analogy for understanding Convex's architecture and why it can't be horizontally scaled without significant rearchitecting.

---

## 1. The Committer = The One Bank Teller

Imagine a bank that has **one teller window**. Every single customer — whether they're depositing, withdrawing, or transferring money — must go through that one teller, one at a time, in a single line.

In Convex, the **Committer** is that teller. Every time your app does something that changes data (inserts a message, updates a user profile, deletes a post), that change gets sent to the Committer. The Committer processes them **one at a time, in order**.

Why? Because the Committer does two critical things:

- **Stamps a number on each change** — like a receipt number. Change #1, Change #2, Change #3. These numbers never go backwards. This is the "monotonically increasing timestamp."
- **Checks that the change is still valid** — "You want to withdraw $500? Let me check your balance... yes, you have $600, approved." If two people try to withdraw from the same account at the same time, the teller catches it because they're processed one at a time.

**Why this blocks horizontal scaling:** If you open a second bank branch (a second Convex node), it has its own teller stamping its own receipt numbers. Now you have two independent tellers who don't know about each other. Person A withdraws $500 at Branch 1, Person B withdraws $500 at Branch 2, both get approved because neither teller saw the other's transaction. The account is now -$400. **Consistency is broken.**

In code terms: the Committer is literally a loop that sits and waits for messages on a channel. One message comes in, it processes it, then the next one. It's in `committer.rs` — a `loop {}` with a single `rx.recv()` (one inbox, one reader).

---

## 2. In-Memory Snapshots = The Teller's Cheat Sheet

When you go to the bank teller, they don't walk to the vault every time to check your balance. They have a **screen in front of them** showing the current state of every account. That screen updates instantly when they process a transaction.

In Convex, the **Snapshot Manager** is that screen. It holds **in RAM** (the computer's fast, temporary working memory — think of it as an open spreadsheet):

- Every table that exists and its metadata
- Every index and what fields it covers
- Document counts, schema shapes, etc.

When a Convex function runs a query like "get all messages in channel #general", it doesn't go to the database on disk. It reads from the snapshot in RAM. This is why Convex is fast — RAM is ~100,000x faster than reading from disk.

**Why this blocks horizontal scaling:** The snapshot is a live, constantly updating picture of the database that lives in **one computer's memory**. If you start a second Convex node, it has its own snapshot — but it's immediately out of date because it doesn't know about the first node's commits. It's like opening a second bank branch where the teller's screen shows yesterday's balances.

Here's the key distinction:

```
Your typical web app (Python on Cloud Run):
  App says: "Hey Postgres, what's the user's balance?"
  Postgres says: "It's $600"
  App says: "OK, set it to $100"
  Postgres says: "Done"
  → The app holds NOTHING in memory. Postgres is the truth.

Convex:
  The truth is IN Convex's memory.
  The disk database (SQLite/Postgres) is just a backup copy.
  → Convex IS the database. It doesn't ask another database.
```

---

## 3. In-Memory Write Log = The Security Camera Tape

Imagine there's a security camera that records every transaction the teller processes. But it's not a permanent archive — it's a short rolling tape that keeps the last few minutes.

The **Write Log** in Convex records every recent change:

- "At timestamp 100, document X was modified"
- "At timestamp 101, document Y was created"
- "At timestamp 102, document X was modified again"

It exists for **two purposes**:

### Purpose 1: Checking for conflicts (OCC)

Before approving a new transaction, the Committer rewinds the tape and asks: "Since this transaction started reading data, has anyone else changed that same data?" If yes, the transaction is rejected and automatically retried.

### Purpose 2: Notifying subscribers (real-time updates)

When your React app subscribes to "list of messages in #general", Convex remembers what documents that query read. Every time the write log gets a new entry, Convex scans it: "Did this write affect any documents that any subscriber cares about?" If yes, it re-runs the query and pushes the new result to the client.

**Why this blocks horizontal scaling:** The write log is an in-memory data structure (`Arc<Mutex<WriteLogManager>>` in Rust terms — but just think of it as a shared notebook in one computer's RAM). Node B can't read Node A's notebook. So:

- Node B can't check if Node A made a conflicting write (breaks consistency)
- Node B can't notify subscribers about Node A's writes (breaks real-time)

---

## 4. Single-Process Subscriptions = One Receptionist Taking All Calls

When your React app uses `useQuery(api.messages.list)`, here's what happens behind the scenes:

1. Your browser opens a **WebSocket** (a persistent two-way connection) to the Convex backend
2. Convex runs your query and sends back the results
3. Convex **remembers** what that query read — "this query read documents #5, #8, and #12 from the messages table"
4. Every time ANY write happens, Convex checks: "Did this write touch documents #5, #8, or #12?"
5. If yes → re-run the query → push new results to your browser instantly

This is how Convex gives you real-time updates without you writing any WebSocket code.

The **subscription state** — the mapping of "Client X is watching documents #5, #8, #12" — lives **in the memory of the Convex process** that client is connected to.

**Why this blocks horizontal scaling:** If Client X is connected to Node A, and a write happens on Node B that changes document #5, Node A has no idea. Client X never gets the update. It's like having two receptionists in two buildings — if someone leaves a message with Receptionist B, Receptionist A can't forward it to their callers.

---

## 5. Single-Node OCC = The "Double-Check Before Approving" System

OCC stands for **Optimistic Concurrency Control**. Here's how it works:

```
1. Your function starts:
   "Let me read the user's balance... it's $600 (at timestamp 100)"

2. Your function does its thing:
   "OK, I want to set it to $100"

3. Before committing, the Committer double-checks:
   "Has anyone changed this user's balance SINCE timestamp 100?"

   - Rewinds the write log
   - Looks for any write to this document after timestamp 100

4a. If NO: "All clear, approved. Here's your receipt: timestamp 105."
4b. If YES: "Someone else changed it at timestamp 103. REJECTED.
     Let's start over with the new data."
     → Convex automatically retries your function from step 1
```

It's called "optimistic" because it **assumes** there won't be a conflict (lets your function run freely) and only checks at the end. The opposite would be "pessimistic" — locking the data before you even read it (like `SELECT ... FOR UPDATE` in SQL).

**Why this blocks horizontal scaling:** Step 3 requires checking the write log, which is local to one process. If another node made a conflicting write, this node's write log doesn't have it, so the conflict goes undetected. Two nodes could both approve conflicting changes to the same document.

---

## Putting It All Together

Here's the full picture:

```
Convex is a bank with:
- ONE teller (Committer) processing all transactions in order
- ONE screen (Snapshot Manager) showing the live state of all accounts
- ONE security tape (Write Log) recording recent transactions
- ONE receptionist (Subscriptions) who calls clients when their accounts change
- ONE rule book (OCC) that says "before approving, check the tape for conflicts"

ALL of these assume there is ONE building.
```

**Horizontal scaling means opening more buildings.** But if each building has its own teller, screen, tape, and receptionist, they can't see each other's work → accounts get corrupted.

**The [proposal](./horizontal-scaling-proposal.md) says: connect the buildings with:**

- A **shared security tape** (distributed log / Kafka)
- A **central receipt number dispenser** (timestamp oracle)
- **Split the accounts** so each building OWNS certain accounts (partitioning)
- Let each building **watch the shared tape** to stay informed (log consumers)
