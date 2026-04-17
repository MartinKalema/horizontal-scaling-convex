# Production Deployment

How to deploy the Convex horizontally-scaled cluster on Kubernetes (GKE, EKS) and bare VMs (EC2). Based on how CockroachDB, TiDB, and YugabyteDB deploy in production.

The Docker Compose setup (`docker compose --profile cluster up`) is for development only — the same pattern all three giants follow. Production uses Kubernetes StatefulSets or bare VM processes with persistent storage.

## Kubernetes (GKE / EKS)

### Architecture

```
                        ┌─────────────────┐
                        │   Cloud NLB     │
                        │  (round-robin)  │
                        └────────┬────────┘
                                 │
              ┌──────────────────┼──────────────────┐
              ▼                  ▼                  ▼
        ┌──────────┐      ┌──────────┐      ┌──────────┐
        │  pod-0   │◄────►│  pod-1   │◄────►│  pod-2   │
        │  PVC-0   │ Raft │  PVC-1   │ Raft │  PVC-2   │
        │(raft-eng)│      │(raft-eng)│      │(raft-eng)│
        └──────────┘      └──────────┘      └──────────┘
              ▲                  ▲                  ▲
              └──── Headless Service (DNS) ────────┘
              pod-0.convex.ns.svc.cluster.local
              pod-1.convex.ns.svc.cluster.local
              pod-2.convex.ns.svc.cluster.local
```

### Key Resources

| Resource | Purpose | What the giants do |
| --- | --- | --- |
| **StatefulSet** | Stable pod names (`pod-0`, `pod-1`) that survive rescheduling | CockroachDB, YugabyteDB both use StatefulSets |
| **Headless Service** | DNS records for Raft peer discovery (`clusterIP: None`) | Universal — all three use this for Raft/gossip |
| **PersistentVolumeClaim** | SSD-backed storage for raft-engine data, bound 1:1 to pods | TiKV forces `Retain` reclaim policy to prevent data loss |
| **StorageClass** | SSD-backed: `pd-ssd` on GKE, `gp3` on EKS | All three mandate SSDs for Raft log latency |

### StatefulSet Configuration

Each partition is a separate StatefulSet with 3 replicas. For our 2-partition cluster:

- `convex-partition-0` — StatefulSet with 3 pods (Raft group for messages, users)
- `convex-partition-1` — StatefulSet with 3 pods (Raft group for projects, tasks)

Each pod gets:
- A stable network identity via the headless service
- A PVC for raft-engine data (`/convex/data/raft-engine`)
- A PVC for PostgreSQL data or a connection to a managed database (RDS, Cloud SQL)

### Peer Discovery

`RAFT_PEERS` is derived from headless DNS names instead of hardcoded IPs:

```
RAFT_PEERS=1=http://convex-p0-0.convex-p0.default.svc.cluster.local:50051,\
           2=http://convex-p0-1.convex-p0.default.svc.cluster.local:50051,\
           3=http://convex-p0-2.convex-p0.default.svc.cluster.local:50051
```

The headless service creates stable DNS A records for each pod. Raft nodes resolve peers via these names. This is how CockroachDB's `--join` flag and YugabyteDB's `--master_addresses` work on Kubernetes.

### Node Identity

`RAFT_NODE_ID` is derived from the StatefulSet pod ordinal:

```
# In the pod entrypoint or init container:
RAFT_NODE_ID=$((${HOSTNAME##*-} + 1))
# pod-0 → RAFT_NODE_ID=1, pod-1 → RAFT_NODE_ID=2, pod-2 → RAFT_NODE_ID=3
```

CockroachDB auto-assigns node IDs on first join and persists them in the store directory. TiKV gets IDs from PD. YugabyteDB uses the pod FQDN as identity. Our approach (ordinal-based) is simplest for StatefulSets.

### Persistent Storage

```yaml
volumeClaimTemplates:
  - metadata:
      name: raft-data
    spec:
      accessModes: ["ReadWriteOnce"]
      storageClassName: pd-ssd  # GKE: pd-ssd, EKS: gp3
      resources:
        requests:
          storage: 10Gi
```

The PVC is bound 1:1 to the pod by ordinal index. If a pod reschedules to a different node, Kubernetes reattaches the same PV. The raft-engine data survives pod restarts — this is what prevents the "to_commit X out of range" panic.

TiKV recommends local SSDs (`local-volume-provisioner`) for lowest Raft log latency. For most workloads, network-attached SSDs (`pd-ssd`, `gp3`) are sufficient.

### Scaling

- **Add nodes**: Increase StatefulSet replica count. The new pod joins with an empty raft-engine store. The Raft leader detects the new follower and replicates entries.
- **Remove nodes**: Reduce replica count. Raft automatically adjusts quorum. Requires updating `RAFT_PEERS` or implementing dynamic membership changes via raft-rs `ConfChange`.

CockroachDB and YugabyteDB both recommend **decommissioning** a node before removal (migrating Raft replicas off it) to avoid under-replication.

## Load Balancing

### Read Traffic

Any node can serve reads (all have the data via NATS delta replication). A standard round-robin NLB distributes read traffic evenly across all pods.

### Write Traffic

Only the Raft leader accepts writes. Two approaches, both used by the giants:

**Option 1: Client-side retry (etcd pattern)**

The NLB sends to any node. If the node isn't the leader, it rejects with "not leader, forward to node X". The client retries to the leader. etcd uses this approach — simple, no server-side forwarding logic.

**Option 2: Server-side forwarding (CockroachDB pattern)**

The follower receives the write, forwards it to the leader internally via gRPC, and returns the result to the client. The client sees a single successful request regardless of which node the NLB chose. CockroachDB calls this "gateway routing."

We already have the gRPC mutation forwarding infrastructure from the replica architecture (`MutationForwarderService`). Reusing it for Raft follower → leader forwarding is straightforward.

### Combined

```
Client ──HTTP──► NLB (round-robin)
                   │
         ┌─────────┼─────────┐
         ▼         ▼         ▼
       pod-0     pod-1     pod-2
      (leader)  (follower) (follower)
         │         │         │
         │    ┌────┘    ┌────┘
         │    │ gRPC    │ gRPC
         │    │ forward │ forward
         ▼    ▼         ▼
       Writes go to leader
       Reads served locally
```

## Bare VMs (EC2)

Same as CockroachDB's EC2 deployment guide:

1. **Process management**: systemd service on each VM
2. **Storage**: EBS volumes (gp3) mounted at `/convex/data` for raft-engine
3. **Peer discovery**: `RAFT_PEERS` contains private IPs (static or from AWS Cloud Map)
4. **Load balancing**: AWS NLB in front of all nodes
5. **Database**: Amazon RDS (PostgreSQL) or self-managed PostgreSQL
6. **Message bus**: Amazon MSK (managed NATS alternative) or self-hosted NATS

```bash
# On each VM:
export RAFT_NODE_ID=1
export RAFT_PEERS="1=http://10.0.1.10:50051,2=http://10.0.1.11:50051,3=http://10.0.1.12:50051"
export NATS_URL="nats://nats.internal:4222"
export POSTGRES_URL="postgresql://convex:password@rds.internal:5432"
./convex-backend
```

## Managed Database (PostgreSQL)

In production, use a managed database instead of a self-hosted PostgreSQL container:

| Cloud | Service | Notes |
| --- | --- | --- |
| GCP | Cloud SQL for PostgreSQL | Private IP, automatic backups |
| AWS | Amazon RDS for PostgreSQL | Multi-AZ for HA, gp3 storage |
| Azure | Azure Database for PostgreSQL | Flexible Server |

Each Convex node needs its own database (separate `INSTANCE_NAME` → separate database). The managed service handles replication, backups, and failover for PostgreSQL itself.

## NATS JetStream

In production, run NATS as a 3-node cluster for HA:

| Cloud | Options |
| --- | --- |
| GCP/AWS/Azure | Self-hosted NATS cluster (3 pods via StatefulSet) |
| Synadia Cloud | Managed NATS-as-a-service |

NATS is lightweight (single Go binary, ~20 MB RAM) and can run alongside the Convex pods or as a separate StatefulSet.

## References

- [Deploy CockroachDB on Kubernetes](https://www.cockroachlabs.com/docs/stable/deploy-cockroachdb-with-kubernetes)
- [Deploy CockroachDB on AWS EC2](https://www.cockroachlabs.com/docs/stable/deploy-cockroachdb-on-aws)
- [TiDB Operator Architecture](https://docs.pingcap.com/tidb-in-kubernetes/stable/architecture/)
- [YugabyteDB on Kubernetes](https://docs.yugabyte.com/stable/deploy/kubernetes/single-zone/aks/statefulset-yaml/)
- [CockroachDB Kubernetes Operator](https://www.cockroachlabs.com/blog/kubernetes-cockroachdb-operator/)
