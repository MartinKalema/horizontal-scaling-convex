# What Problem Does the Primary-Replica Approach Fix?

**It fixes the problem of too many people trying to read at the same time.**

---

## The Current Situation

Right now, Convex is a single server doing everything:

- Processing all writes (mutations)
- Answering all reads (queries)
- Holding all WebSocket connections (subscriptions)
- Keeping all indexes and snapshots in its RAM

That one server has a ceiling. It has a fixed amount of RAM, a fixed number of CPU cores, and a fixed number of network connections it can hold. When your app grows, reads and subscriptions are what grow fastest — every user on your app is running queries and holding a WebSocket connection, but only a fraction of them are writing data at any given moment.

---

## What the Primary-Replica Approach Does

| Problem | How It's Fixed |
|---|---|
| 10,000 users all running queries against one server's CPU/RAM | Spread them across 4 Replica nodes. Each node handles 2,500 users. |
| 50,000 WebSocket connections saturating one server's network | Spread them across Replicas. Each node holds 12,500 connections. |
| In-memory indexes consuming 32 GB of RAM on one machine | Each Replica has its own 32 GB. You're no longer limited to one machine's memory. |
| The server spending CPU time serving reads when it should be processing writes | The Primary is now dedicated to writes. It doesn't serve queries or hold subscriptions. It commits faster because it's not competing for resources. |

---

## What It Does NOT Fix

- **Write throughput** — still one Committer, still one server processing mutations. But that's thousands of mutations/sec, which is more than enough for the vast majority of apps.

---

## In One Sentence

The primary-replica approach lets you add more servers to handle more readers, while keeping one server in charge of writing so you don't break consistency.
