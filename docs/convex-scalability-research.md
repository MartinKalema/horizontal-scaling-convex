# Convex Scalability: What the Community and Official Docs Say

A comprehensive collection of what developers, the Convex team, and the broader community say about Convex's scalability — sourced from official documentation, GitHub issues, Hacker News, blog posts, and developer forums.

---

## Table of Contents

1. [Official Platform Limits](#1-official-platform-limits)
2. [How Convex Partially Solved Horizontal Scaling: Funrun](#2-how-convex-partially-solved-horizontal-scaling-funrun)
3. [Self-Hosted: Single-Node by Design](#3-self-hosted-single-node-by-design)
4. [Community Concerns: GitHub Issues](#4-community-concerns-github-issues)
5. [Hacker News Discussions](#5-hacker-news-discussions)
6. [Criticisms from convex.sucks](#6-criticisms-from-convexsucks)
7. [Convex's Own Position on Benchmarks](#7-convexs-own-position-on-benchmarks)
8. [Convex vs Competitors: Scalability Comparison](#8-convex-vs-competitors-scalability-comparison)
9. [Real Developer Experiences](#9-real-developer-experiences)
10. [Sources](#10-sources)

---

## 1. Official Platform Limits

Convex publishes hard limits that vary by deployment class. These are the ceilings your app runs into before you can request increases.

### Deployment Classes (Concurrency)

| Class | Concurrent Queries | Concurrent Mutations | Concurrent Actions | Mutation Write Throughput |
|-------|-------------------|---------------------|--------------------|-----------------------|
| S16   | 16                | 16                  | 64                 | 4 MiB                |
| S256  | 256               | 256                 | 256                | 8 MiB                |
| D1024 | 1024              | 512                 | 1024               | 32 MiB               |

### Database Limits

| Limit | Value |
|-------|-------|
| Storage (Free/Starter) | 0.5 GiB included |
| Storage (Professional) | 50 GiB included |
| Bandwidth (Free/Starter) | 1 GiB/month |
| Bandwidth (Professional) | 50 GiB/month |
| Tables per deployment | 10,000 |
| Indexes per table | 32 |
| Document size | 1 MiB |
| Fields per document | 1,024 |

### Transaction Limits

| Limit | Value |
|-------|-------|
| Data read per transaction | 16 MiB |
| Data written per transaction | 16 MiB |
| Documents scanned per transaction | 32,000 |
| Documents written per transaction | 16,000 |
| Index ranges read | 4,096 |
| Query/mutation execution time | 1 second |
| Action execution time | 10 minutes |

### Function Calls

| Limit | Free/Starter | Professional |
|-------|-------------|-------------|
| Calls per month | 1M | 25M |
| Action execution | 20 GiB-hours | 250 GiB-hours |

Paid plans have no hard resource limits on storage — they can scale to billions of documents and TBs of storage. Concurrency limits can be increased for Business and Enterprise customers on a case-by-case basis.

**Source:** [Convex Limits Documentation](https://docs.convex.dev/production/state/limits)

---

## 2. How Convex Partially Solved Horizontal Scaling: Funrun

Convex has acknowledged the scaling ceiling and addressed **one piece** of it: function execution.

### The Old Problem

Convex originally ran functions inside the backend process using V8 (Google's JavaScript engine). V8 has a limit of **128 threads**, so a single backend could only run **128 functions concurrently**. This was a hard ceiling.

### The Solution: Funrun (March 2024)

The team built **Funrun**, a separate multi-tenant gRPC service that handles function execution independently from the backend:

> "Funrun is a read-only gRPC service that reads from the database to get a snapshot of the data, computes the results of developer-defined functions, and sends the results back to the backend."

Key improvements:
- **10x concurrency increase** for Pro customers
- **Median latency under 20ms** maintained
- V8 isolates spin up in **10ms** (vs 500ms-10s for AWS Lambda cold starts)
- Uses **Rendezvous hashing** to route requests consistently and maximize cache hits
- Funrun instances can be **scaled horizontally** independently

### What This Does NOT Solve

Funrun only scales the **compute layer** (running your JavaScript/TypeScript functions). The core bottlenecks remain:
- The **Committer** is still single-threaded and serializes all writes
- The **Snapshot Manager** still holds ground truth in a single process's RAM
- **Subscriptions** are still per-instance
- The **database itself** does not scale horizontally

Think of it like adding more cashiers (Funrun) to a store that still has only one warehouse (the database/committer). Checkout is faster, but the warehouse is still the bottleneck.

**Source:** [How We Horizontally Scaled Function Execution](https://stack.convex.dev/horizontally-scaling-functions)

---

## 3. Self-Hosted: Single-Node by Design

Since Convex went open-source, self-hosting has been an option. But the documentation is explicit about the scaling constraints.

### Official Statements

> "By default, the self-hosted backend needs to run on a single machine."

> "If you want to scale it, you'll need to modify the open-source backend code to scale the various services for your own infrastructure."

> "Operating self-hosted Convex may end up being a lot of work, and if your project starts getting adopted and needs scale, seamless migrations, automatic backups, and high availability, the cloud service is strongly recommended."

### Self-Hosted vs Cloud

| Capability | Self-Hosted | Cloud |
|-----------|------------|-------|
| Horizontal function scaling | Not built-in | Yes (Funrun) |
| High availability | Manual (you replicate, snapshot, monitor) | Managed |
| Text search distribution | Single node | Distributed |
| WebSocket distribution | Single node | Distributed |
| Support plan | None | Available |
| Database proximity | You manage co-location | Co-located (~1ms single-record queries) |

The docs recommend the backend be **"in the same region and as close as possible to the database"** — acknowledging that latency between backend and database directly impacts app speed.

### Failure Recovery

> "All machines fail eventually and how you respond is important."

Self-hosters must manually configure database replication, snapshots, monitoring, alerting, and automatic restart mechanisms. Each backend upgrade may also require running database migrations between versions.

**Source:** [Self-Hosting with Convex](https://stack.convex.dev/self-hosted-develop-and-deploy)

---

## 4. Community Concerns: GitHub Issues

### Issue #95: Bandwidth, Caching, and Scalability

A developer (franckdsf) raised detailed concerns after migrating their app to Convex:

**Bandwidth blowup:**
- Database was only 5 MB with 6 MB writes, but reads hit **900 MB in one month**
- Projected to **600+ GB/month** at scale — compared to ~3 GB/month on their previous Supabase setup

**Aggressive cache invalidation:**
- Updating any field on a document invalidates **all queries** that touch that document, even queries that don't read the changed field
- Example: updating `userIds` on a workspace invalidates queries that only fetch the `name` field
- Paginated queries "never seem to use the cache"

**Convex team response (ikhare):**
- Confirmed Convex uses **document-level** invalidation, not field-level
- Recommended using helper libraries for many-to-many relationships
- Described the model as "a relational data model...with a TypeScript query language and not SQL"

**Source:** [GitHub Issue #95](https://github.com/get-convex/convex-backend/issues/95)

### Issue #188: Performance and Benchmarks

A developer (lbensaad) reported that inserting **100,000 simple records** took **several minutes** in Convex, compared to **less than 1 minute** in PocketBase or Trailbase.

**Convex team response (nipunn1313):**
- Acknowledged the request for benchmarks as "totally reasonable"
- Noted that each Convex mutation is a full transaction, so the comparison may not be apples-to-apples
- Stated they don't have public benchmark results yet: "it's on our minds to get to eventually"
- Pointed to a self-hosted benchmarking guide for community members to run their own tests

**Source:** [GitHub Issue #188](https://github.com/get-convex/convex-backend/issues/188)

---

## 5. Hacker News Discussions

### "How Convex Works" (April 2024)

A detailed technical discussion where Convex co-founder **James Cowling** participated.

**Architecture insights:**
- The database transaction logic, timestamp assignment, and concurrency control are all **custom-built** — not built on Kafka or other off-the-shelf systems
- The open-source backend uses **SQLite** as the write-ahead log foundation
- Phantom read anomalies are addressed through **data range-based readsets** rather than object lists

**Community praise:**
- Polished developer experience highlighted as a standout
- Real-time apps without manual WebSocket configuration praised

**Community concerns:**
- **Scalability questions** about unique constraints and large-scale operations
- **Vendor lock-in** fears about long-term business viability
- Convex addressed lock-in by open-sourcing under **FSL** (becoming Apache 2.0 after two years)

**Source:** [HN: How Convex Works](https://news.ycombinator.com/item?id=40020516)

### "Why did you use Convex as backend?" (2025)

A YC S25 company (VibeFlow) explained their choice:
- **Project isolation**: "We spin up isolated projects for each user. Convex handles this seamlessly with zero manual setup"
- **Reactivity**: Built-in reactive capabilities keep UIs synchronized automatically
- **Type safety**: "Everything is end-to-end TypeScript with transactions by default"
- Compared favorably to Supabase and Firebase for their specific workflow use case

**Source:** [HN: Why did you use Convex as backend?](https://news.ycombinator.com/item?id=45085274)

---

## 6. Criticisms from convex.sucks

A community-maintained page cataloging Convex's limitations:

### Scalability-Related
- **No giant map-reduce queries** — requires streaming data elsewhere
- **No native analytics support** (promised "real soon")
- **No SQL support** — users must stream data out to an analytics database via Fivetran or Airbyte
- **`.filter()` operations are unindexed table scans** — easy to accidentally write slow queries
- **Application-level joins** cross the TypeScript-database boundary, creating inefficiency (though `Promise.all` helps)

### General Limitations
- No geospatial indexing
- No database triggers
- No push notifications
- No native Stripe integration
- Joins in JavaScript are described as "a little clumsy and it's extra code to write"

### Business Concerns
- **Vendor lock-in**: "If you love Convex and build your product on it then you depend on it"
- **Skill transferability**: Learning Convex-specific patterns lacks portability compared to PostgreSQL, Terraform, or Kafka
- **Pricing uncertainty**: Cost estimation is difficult for scaling applications at $25/month Pro tier

**Source:** [Convex SUCKS!](https://www.convex.sucks/)

---

## 7. Convex's Own Position on Benchmarks

In a blog post titled "I don't care about your database benchmarks (and neither should you)", the Convex team laid out their philosophy:

### Key Statements

> "Does that mean Convex will be the absolute fastest in every benchmark vs. every competitor? I mean, almost certainly not. Let's just say no."

> "I'm confident we can pull off any move our competitors can" — but immediately acknowledges competitors have equivalent capabilities: "There are great database engineers working at all these companies."

### Their Argument
- Performance comparisons "always amount to apples vs. anvils" — different systems measure different things
- Historical examples of inflated benchmarks: MongoDB and Redis used questionable practices (periodic disk writes, delayed acknowledgments)
- They plan to publish **real-world scenarios** instead of synthetic benchmarks, including:
  1. Maximum request rates maintaining acceptable tail latencies
  2. Monthly operational costs for those loads
  3. Historical performance trends over time

**Source:** [I don't care about your database benchmarks](https://stack.convex.dev/on-competitive-benchmarks)

---

## 8. Convex vs Competitors: Scalability Comparison

### Convex vs Supabase

| Dimension | Convex | Supabase |
|-----------|--------|----------|
| Real-time latency at 5K connections | Sub-50ms read/write | 100-200ms p99 |
| Scaling model | Vertical (single node) + Funrun for compute | Vertical on hosted; horizontal if self-hosted |
| Query language | TypeScript functions | SQL (PostgreSQL) |
| Real-time | Built-in, automatic | Requires Realtime extension |
| Self-hosted scaling | Must modify source code | PostgreSQL-native horizontal tools available |

### Convex vs PlanetScale

| Dimension | Convex | PlanetScale |
|-----------|--------|-------------|
| Architecture | Single-node stateful | Vitess-based distributed MySQL |
| Horizontal scaling | Not supported (except Funrun) | Native via Vitess sharding |
| Throughput | No published benchmarks | ~17,000 QPS (TPCC), ~35,000 QPS (OLTP read-only) |
| Real-time | Built-in | Not built-in |
| Schema changes | Automatic | Non-blocking DDL |

### When Convex Wins
- Real-time collaborative apps where sub-50ms reactivity matters
- Rapid prototyping with end-to-end TypeScript
- Apps where developer experience > raw throughput

### When Competitors Win
- Apps requiring horizontal write scaling (PlanetScale, CockroachDB)
- Apps needing SQL and analytics (Supabase, PlanetScale)
- Apps where vendor-agnostic architecture is a priority

**Sources:**
- [Convex vs Supabase 2025 comparison](https://makersden.io/blog/convex-vs-supabase-2025)
- [PlanetScale vs Supabase benchmarks](https://planetscale.com/benchmarks/supabase)
- [Best Convex Alternatives 2026](https://www.buildmvpfast.com/alternatives/convex)

---

## 9. Real Developer Experiences

### Positive: Scaling to 70 Modules

A developer documented building web, mobile, and API backends on a single Convex deployment with 70 modules. The takeaway:

> "There isn't a huge catch when an app scales on Convex, apart from having to remember to add indexes where needed."

**Source:** [Scaling to 70 Modules](https://dev.to/convexchampions/scaling-to-70-modules-building-a-web-mobile-and-api-backend-on-one-convex-deployment-3pcg)

### Positive: Self-Hosting as Personal API Layer

A developer chose to self-host Convex as a personal API layer, finding it suitable for individual/small-scale use where horizontal scaling isn't needed.

**Source:** [Why I Self-Host Convex as My Personal API Layer](https://dev.to/ryancwynar/why-i-self-host-convex-as-my-personal-api-layer-41h5)

### Negative: Bandwidth Surprise

The developer from GitHub Issue #95 hit 900 MB reads from a 5 MB database in one month, driven by aggressive cache invalidation and paginated query re-fetching. Their Supabase setup handled the same workload at ~3 GB/month.

### Negative: Bulk Insert Performance

From GitHub Issue #188, inserting 100K records took "several minutes" vs under 1 minute on competing platforms. The Convex team acknowledged this is due to every mutation being a full serializable transaction.

### Convex Team Roadmap Response

Convex CEO James Cowling shared a roadmap update addressing scaling:
- Increasing function execution limits, parallelism, and size limits
- Moving all backends to new high-scale infrastructure
- Continuing to expand Funrun's horizontal scaling capabilities

**Source:** [James Cowling Roadmap Update (X/Twitter)](https://x.com/jamesacowling/status/1887346879043936607)

---

## 10. Sources

All information in this document was obtained from the following URLs:

### Official Convex Documentation
- [Convex Limits](https://docs.convex.dev/production/state/limits)
- [Convex Best Practices](https://docs.convex.dev/understanding/best-practices/)
- [Convex Tutorial: Scaling Your App](https://docs.convex.dev/tutorial/scale)
- [Rate Limiting Documentation](https://docs.convex.dev/agents/rate-limiting)

### Convex Engineering Blog (stack.convex.dev)
- [How We Horizontally Scaled Function Execution](https://stack.convex.dev/horizontally-scaling-functions)
- [Self-Hosting with Convex: Everything You Need to Know](https://stack.convex.dev/self-hosted-develop-and-deploy)
- [I don't care about your database benchmarks](https://stack.convex.dev/on-competitive-benchmarks)
- [Queries That Scale](https://stack.convex.dev/queries-that-scale)
- [Rate Limiting at the Application Layer](https://stack.convex.dev/rate-limiting)
- [Why I Picked Convex over Firebase, Supabase, and Neon](https://stack.convex.dev/why-choose-convex-database-for-backend)

### GitHub
- [Issue #95: Bandwidth, Caching, and Scalability Limits](https://github.com/get-convex/convex-backend/issues/95)
- [Issue #188: Performance and Benchmarks](https://github.com/get-convex/convex-backend/issues/188)
- [Convex Backend Repository](https://github.com/get-convex/convex-backend)

### Community Discussions
- [HN: How Convex Works](https://news.ycombinator.com/item?id=40020516)
- [HN: Why did you use Convex as backend?](https://news.ycombinator.com/item?id=45085274)
- [HN: Convex vs Firebase](https://news.ycombinator.com/item?id=31831623)
- [HN: Opencom with Convex Backend](https://news.ycombinator.com/item?id=47194196)
- [Convex SUCKS!](https://www.convex.sucks/)

### Comparisons and Reviews
- [Convex vs Supabase 2025 - Makers Den](https://makersden.io/blog/convex-vs-supabase-2025)
- [Best Convex Alternatives 2026](https://www.buildmvpfast.com/alternatives/convex)
- [PlanetScale vs Supabase Benchmarks](https://planetscale.com/benchmarks/supabase)

### Developer Experience Posts
- [Scaling to 70 Modules on Convex](https://dev.to/convexchampions/scaling-to-70-modules-building-a-web-mobile-and-api-backend-on-one-convex-deployment-3pcg)
- [Why I Self-Host Convex](https://dev.to/ryancwynar/why-i-self-host-convex-as-my-personal-api-layer-41h5)
- [Self-Hosted Convex on AWS with SST](https://seanpaulcampbell.com/blog/self-hosted-convex-aws-sst/)
- [Convex Goes Open-Source Announcement](https://news.convex.dev/convex-goes-open-source/)

### Social
- [James Cowling (Convex CEO) Roadmap Update](https://x.com/jamesacowling/status/1887346879043936607)

---

*Research conducted on March 31, 2026.*
