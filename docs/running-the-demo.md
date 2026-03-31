# Running and Testing the Replication Demo

## What Works Today

### Unit/Integration Tests (run locally with cargo)

The replication pipeline is tested at the database crate level:

```sh
# Run just the replication test
cargo test -p database "test_primary_commit_publishes_delta"

# Run all database tests (337 tests, includes replication)
cargo test -p database
```

The test proves: Primary commits a document → CommitDelta is published to InMemoryDistributedLog → delta contains the correct document updates.

### Single-Node Primary with PostgreSQL (Docker)

You can run the Convex backend with PostgreSQL today — this is the existing self-hosted setup, not yet replicated:

```sh
cd self-hosted/docker

# Start PostgreSQL + Primary + Dashboard
docker compose -f docker-compose.replicated.yml up
```

This starts:
- **PostgreSQL** on port 5432
- **Primary backend** on port 3210 (API) and 3211 (site proxy)
- **NATS JetStream** on port 4222 (ready for when we wire replication)
- **Dashboard** on port 6791

Generate an admin key:
```sh
docker compose -f docker-compose.replicated.yml exec primary ./generate_admin_key.sh
```

Visit the dashboard at http://localhost:6791.

## What's Not Wired Yet

The Replica service is commented out in docker-compose.replicated.yml because the backend binary doesn't yet support these features:

| Feature | Status | What's Needed |
|---|---|---|
| `--replica` mode flag | Not started | Add CLI flag to `local_backend` |
| NATS DistributedLog | Not started | Implement `DistributedLog` trait for NATS JetStream |
| Replica startup | Not started | Wire `ReplicaSnapshotUpdater` into backend startup |
| Mutation forwarding | Not started | Wire gRPC client to forward mutations to Primary |
| Read-after-write consistency | Not started | Timestamp tracking in client requests |

## Architecture When Complete

```
                    ┌──────────────┐
                    │  PostgreSQL  │
                    │  (shared)    │
                    └──────┬───────┘
                           │
              ┌────────────┼────────────┐
              │            │            │
        ┌─────┴─────┐     │     ┌──────┴────┐
        │  Primary  │     │     │  Replica  │
        │  :3210    │     │     │  :3220    │
        │  writes   │     │     │  reads    │
        └─────┬─────┘     │     └──────┬────┘
              │            │            │
              │     ┌──────┴──────┐     │
              └────►│    NATS     │◄────┘
                    │  JetStream  │
                    │  :4222      │
                    └─────────────┘
```

## Next Steps to Get Full Replication Running

1. **Implement NatsDistributedLog** — publish/subscribe to NATS JetStream
2. **Add replication mode to local_backend** — `REPLICATION_MODE=primary|replica` env var
3. **Wire ReplicaSnapshotUpdater into backend startup** for replica mode
4. **Uncomment the replica service** in docker-compose.replicated.yml
5. **Test end-to-end**: write on Primary, verify data appears on Replica
