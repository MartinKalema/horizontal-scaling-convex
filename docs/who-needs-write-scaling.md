# Who Actually Needs Write Scaling?

Write scaling means the ability to process more database writes (inserts, updates, deletes) per second than a single server can handle. This is a different problem from read scaling, and far fewer applications actually need it.

---

## Apps That Need Write Scaling

### High-Frequency Ingestion

- **IoT platforms** — millions of sensors reporting every second
- **Ad tech** — bidding events, impression tracking — billions of writes/day
- **Telemetry/observability** (Datadog, Grafana Cloud) — every metric is a write

### Financial Systems

- **Stock exchanges and trading platforms** — NYSE processes ~3 billion messages/day
- **Payment processors** (Stripe, Square) — every charge, refund, and webhook is a write
- **Betting/gambling platforms** — odds update constantly, every bet is a write

### Multiplayer/Real-Time at Massive Scale

- **Games like Fortnite, Roblox** — millions of concurrent players, every action is a state mutation
- **Collaborative editing at Google Docs scale** — every keystroke is a write
- **Discord-scale chat** — 150M+ monthly users, every message is a write

### Social Platforms

- **Twitter/X at peak** — 12,000+ tweets/sec, plus likes, retweets, bookmarks
- **Instagram** — 95M+ photos/day, each with metadata writes
- **TikTok** — comments, likes, views — all writes, at enormous volume

### E-Commerce at Peak

- **Amazon on Prime Day, Shopify on Black Friday** — inventory decrements, order creation, payment records — all concurrent writes to hot data

---

## The Honest Truth for Convex's Target Audience

Almost none of the above are Convex apps. Convex is used for:

- SaaS products
- Collaborative tools
- Internal apps
- AI-powered products
- Real-time dashboards

These are **overwhelmingly read-heavy**. A SaaS app with 10,000 users might do 50-100 mutations/sec at peak. The single Committer handles that without breaking a sweat.

You'd need write scaling if you were building the **next Discord** or the **next Robinhood** on Convex. At which point you'd also have a team of 50+ engineers and the resources to implement the [partitioned write architecture](./horizontal-scaling-proposal.md).

The [primary-replica approach](./actual-implementation-plan.md) handles everything else.
