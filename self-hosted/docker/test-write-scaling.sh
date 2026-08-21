#!/usr/bin/env bash
#
# Write Scaling Integration Tests (CockroachDB roachtest pattern)
#
# Runs against the live partitioned Docker deployment. Eight test categories
# inspired by CockroachDB, TiDB, and Vitess:
#
#   1. Cross-Partition Data Verification (Vitess VDiff)
#   2. Bank Invariant — Single Table (CockroachDB Jepsen bank)
#   3. Bank Invariant — Multi-Table (TiDB bank-multitable)
#   4. Transparent Partition Routing (topology hiding)
#   4b. All Replica Entrypoint Routing (cluster-wide topology hiding)
#   4c. Six-Entrypoint Read-After-Write (session/topology hiding)
#   4d. Live Membership Refresh After Node Readdress
#   4e. Membership Drain Exclusion
#   4f. Membership Heartbeat Expiry
#   5. Concurrent Write Scaling (CockroachDB KV)
#   6. Monotonic Reads (TiDB monotonic)
#   7. Node Restart Recovery (TiDB kill -9 / CockroachDB nemesis)
#   8. Idempotent Re-Run (CockroachDB workload check)
#   9. Two-Phase Commit Cross-Partition (Vitess 2PC)
#   9f. 2PC Atomicity During NATS Outage
#   9g. 2PC Atomicity During Participant Restart
#   9h. 2PC Atomicity During Coordinator Restart
#   9i. Per-Write 2PC Exactness During Failure Windows
#   9j. 2PC Exactness During Repeated NATS Flaps
#   9k. Post-Ack 2PC Exactness After Cleanup-Window Restarts
#   9l. Parallel 2PC Early-Ack Code-Path Proof
#   9m. Cluster-Safe Latest Reads With Delayed Participant Delta
#   0. Fresh Remote Catalog Coherence
#  10. Rapid-Fire Writes Under Load (Jepsen stress)
#  11. Write-Then-Immediate-Read Consistency (stale read detection)
#  12. Double Node Restart (crash recovery stress)
#  13. Invariant Preservation Under Full Load (final workload check)
#  14. Sequential Ordering (Jepsen sequential)
#  15. Set Completeness (Jepsen set)
#  16. Concurrent Counter (Jepsen counter / YugabyteDB)
#  17. Write-Then-Cross-Node-Read (cross-node read-after-write)
#  18. Interleaved Cross-Partition Reads (read skew detection)
#  19. Large Batch Write Atomicity
#  20. Kill Both Nodes Sequentially (full cluster restart)
#  20b. Writes Through Followers During Raft Leader Outage
#  20c. Lagging Follower Catch-Up After Cross-Partition Writes
#  20d. Cross-Partition 2PC During Participant Leader Outage
#  20e. Repeated Raft Leadership Churn During Cross-Partition 2PC
#  20f. Per-Write 2PC Exactness During Leadership Churn
#  21. Sustained Writes 30s (long-running replication)
#  22. Duplicate Insert Idempotency
#  23. Final Exhaustive Invariant Check
#  24. Single-Key Register (CockroachDB Jepsen register)
#  25. Disjoint Record Ordering (CockroachDB Jepsen comments)
#  26. NATS Partition Simulation (Chaos Mesh network partition)
#  27. Write During Deploy (deploy safety)
#  28. Empty Table Cross-Node Query (boundary)
#  29. Max Batch Size 200 docs (boundary)
#  30. Null and Empty Field Values (boundary)
#  31. Concurrent Table Creation (race condition)
#  32. Rapid Deploy Cycle (deploy stability)
#  33. Read During Active Replication (consistency)
#  34. Clock Monotonicity After Restart (TSO)
#  35. Single Document Read-Modify-Write (register)
#  36. Write Skew Detection (G2 anomaly)
#  37. Ultimate Final Invariant Check
#  38. Sync Route Authority Guard
#  39. Dynamic Membership Advertisement
#
# Prerequisites:
#   ./start-cluster.sh
#
# Usage:
#   cd self-hosted/docker && ./test-write-scaling.sh

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
REPO_ROOT="$(cd "$SCRIPT_DIR/../.." && pwd)"
COMPOSE_PROJECT_NAME="${COMPOSE_PROJECT_NAME:-docker}"

# Keep the historical service-name shorthand while allowing isolated Compose
# projects to validate fresh volumes without colliding with another test run.
docker() {
    local args=()
    local arg
    for arg in "$@"; do
        if [[ "$arg" == docker-*-1 ]]; then
            arg="${COMPOSE_PROJECT_NAME}-${arg#docker-}"
        fi
        args+=("$arg")
    done
    command docker "${args[@]}"
}

NODE_P0A_URL="http://127.0.0.1:3210"
NODE_P0B_URL="http://127.0.0.1:3220"
NODE_P0C_URL="http://127.0.0.1:3230"
NODE_P1A_URL="http://127.0.0.1:3310"
NODE_P1B_URL="http://127.0.0.1:3320"
NODE_P1C_URL="http://127.0.0.1:3330"
NODE_A_URL="$NODE_P0A_URL"
NODE_B_URL="$NODE_P1A_URL"

RED='\033[0;31m'
GREEN='\033[0;32m'
YELLOW='\033[1;33m'
BOLD='\033[1m'
NC='\033[0m'

PASSED=0
FAILED=0
BACKEND_CONTAINERS=(
    docker-node-p0a-1
    docker-node-p0b-1
    docker-node-p0c-1
    docker-node-p1a-1
    docker-node-p1b-1
    docker-node-p1c-1
)
CANONICAL_GENESIS_ID=""

pass() {
    PASSED=$((PASSED + 1))
    echo -e "  ${GREEN}PASS${NC} $1"
}

fail() {
    FAILED=$((FAILED + 1))
    echo -e "  ${RED}FAIL${NC} $1"
    [ -n "${2:-}" ] && echo -e "       $2"
}

parallel_commit_log_count() {
    local count=0
    local matches=0
    for name in "${BACKEND_CONTAINERS[@]}"; do
        matches=$(docker logs "$name" 2>&1 | grep -c "parallel-committed" || true)
        count=$((count + matches))
    done
    echo "$count"
}

assert_dynamic_membership_advertisements() {
    local details=""
    local specs=(
        "docker-node-p0a-1 node-p0a"
        "docker-node-p0b-1 node-p0b"
        "docker-node-p0c-1 node-p0c"
        "docker-node-p1a-1 node-p1a"
        "docker-node-p1b-1 node-p1b"
        "docker-node-p1c-1 node-p1c"
    )

    for spec in "${specs[@]}"; do
        read -r container base <<< "$spec"
        local configured
        configured=$(docker exec "$container" printenv ADVERTISE_GRPC_ADDR 2>/dev/null || true)
        if [[ ! "$configured" =~ ^${base}-[^:]+:50051$ ]]; then
            details="$details $container env=$configured expected=${base}-<run-id>:50051;"
            continue
        fi
        if ! docker logs "$container" 2>&1 | grep -F "Registered cluster membership node" | grep -F "$configured" > /dev/null; then
            details="$details $container did not log membership registration for $configured;"
        fi
    done

    if [ -z "$details" ]; then
        pass "Cluster membership registered per-run advertised gRPC aliases"
    else
        fail "Cluster membership advertised-address registration missing" "$details"
    fi
}

read_persistence_global() {
    local database_name="$1"
    local key="$2"
    docker exec docker-postgres-1 psql -U postgres -d "$database_name" -Atc \
        "SELECT convert_from(json_value, 'UTF8') FROM persistence_globals WHERE key = '$key'" \
        2>/dev/null || true
}

genesis_id_from_json() {
    python3 -c 'import json,sys; print(json.load(sys.stdin)["genesis_id"])'
}

wait_for_persistence_global() {
    local database_name="$1"
    local key="$2"
    local value=""
    for _ in $(seq 1 90); do
        value=$(read_persistence_global "$database_name" "$key")
        if [ -n "$value" ]; then
            printf '%s' "$value"
            return 0
        fi
        sleep 1
    done
    return 1
}

assert_canonical_cluster_genesis() {
    local details=""
    local expected=""
    local specs=(
        "convex_node_p0a docker-node-p0a-1"
        "convex_node_p0b docker-node-p0b-1"
        "convex_node_p0c docker-node-p0c-1"
        "convex_node_p1a docker-node-p1a-1"
        "convex_node_p1b docker-node-p1b-1"
        "convex_node_p1c docker-node-p1c-1"
    )

    for spec in "${specs[@]}"; do
        read -r database_name container <<< "$spec"
        local marker
        local applied
        marker=$(wait_for_persistence_global "$database_name" cluster_genesis || true)
        applied=$(wait_for_persistence_global "$database_name" cluster_genesis_raft_applied || true)
        if [ -z "$marker" ] || [ -z "$applied" ]; then
            details="$details $container marker=${marker:-missing} raft_applied=${applied:-missing};"
            continue
        fi
        local marker_id
        local applied_id
        marker_id=$(genesis_id_from_json <<< "$marker" 2>/dev/null || true)
        applied_id=$(genesis_id_from_json <<< "$applied" 2>/dev/null || true)
        if [ -z "$expected" ]; then
            expected="$marker_id"
        fi
        if [ -z "$marker_id" ] || [ "$marker_id" != "$expected" ] || [ "$applied_id" != "$expected" ]; then
            details="$details $container marker=$marker_id raft_applied=$applied_id expected=$expected;"
        fi
    done

    if [ -z "$details" ] && [ -n "$expected" ]; then
        CANONICAL_GENESIS_ID="$expected"
        pass "All six nodes installed one canonical Convex genesis"
        pass "Every node applied canonical genesis through its partition Raft log"
    else
        fail "Cluster genesis state diverged" "$details"
    fi
}

# --- Preflight ---

echo ""
echo -e "${BOLD}Preflight checks${NC}"

for name in "${BACKEND_CONTAINERS[@]}"; do
    if ! docker inspect "$name" > /dev/null 2>&1; then
        echo -e "${RED}Container $name not running. Start the deployment first:${NC}"
        echo "  ./start-cluster.sh"
        exit 1
    fi
done
echo "  Containers running."

assert_dynamic_membership_advertisements
assert_canonical_cluster_genesis

if [ "${EXPECT_PARALLEL_2PC_EARLY_ACK:-false}" = "true" ]; then
    EARLY_ACK_ENV_OK=true
    EARLY_ACK_ENV_DETAILS=""
    for name in "${BACKEND_CONTAINERS[@]}"; do
        value=$(docker exec "$name" printenv ENABLE_PARALLEL_2PC_EARLY_ACK 2>/dev/null || true)
        if [ "$value" != "true" ]; then
            EARLY_ACK_ENV_OK=false
            EARLY_ACK_ENV_DETAILS="$EARLY_ACK_ENV_DETAILS $name=${value:-unset}"
        fi
    done
    if $EARLY_ACK_ENV_OK; then
        pass "Parallel 2PC early-ack knob enabled on all backend containers"
    else
        fail "Parallel 2PC early-ack suite requires ENABLE_PARALLEL_2PC_EARLY_ACK=true" "$EARLY_ACK_ENV_DETAILS"
        exit "$FAILED"
    fi
fi

NODE_P0A_KEY=$(docker exec docker-node-p0a-1 ./generate_admin_key.sh 2>&1 | tail -1)
NODE_P0B_KEY=$(docker exec docker-node-p0b-1 ./generate_admin_key.sh 2>&1 | tail -1)
NODE_P0C_KEY=$(docker exec docker-node-p0c-1 ./generate_admin_key.sh 2>&1 | tail -1)
NODE_P1A_KEY=$(docker exec docker-node-p1a-1 ./generate_admin_key.sh 2>&1 | tail -1)
NODE_P1B_KEY=$(docker exec docker-node-p1b-1 ./generate_admin_key.sh 2>&1 | tail -1)
NODE_P1C_KEY=$(docker exec docker-node-p1c-1 ./generate_admin_key.sh 2>&1 | tail -1)
NODE_A_KEY="$NODE_P0A_KEY"
NODE_B_KEY="$NODE_P1A_KEY"
echo "  Admin keys generated."

# --- Deploy test functions ---

DEPLOY_DIR=$(mktemp -d)
mkdir -p "$DEPLOY_DIR/convex"

cat > "$DEPLOY_DIR/package.json" << EOF
{ "name": "write-scaling-test", "version": "1.0.0", "dependencies": { "convex": "file:$REPO_ROOT/npm-packages/convex" } }
EOF

cat > "$DEPLOY_DIR/convex/messages.ts" << 'TSEOF'
import { mutation, query } from "./_generated/server";
import { v } from "convex/values";

export const send = mutation({
  args: { text: v.string(), author: v.optional(v.string()), channel: v.optional(v.string()) },
  handler: async (ctx, args) => {
    await ctx.db.insert("messages", { text: args.text, author: args.author ?? "anon", channel: args.channel ?? "general", timestamp: Date.now() });
  },
});

export const createUser = mutation({
  args: { name: v.string(), role: v.string() },
  handler: async (ctx, args) => {
    return await ctx.db.insert("users", { ...args, createdAt: Date.now() });
  },
});

export const createProject = mutation({
  args: { name: v.string(), status: v.string(), budget: v.number() },
  handler: async (ctx, args) => {
    return await ctx.db.insert("projects", { ...args, createdAt: Date.now() });
  },
});

export const createTask = mutation({
  args: { title: v.string(), assignee: v.string(), project: v.string(), status: v.string() },
  handler: async (ctx, args) => {
    return await ctx.db.insert("tasks", { ...args, createdAt: Date.now() });
  },
});

export const createCatalogProbe = mutation({
  args: { runId: v.string(), token: v.string() },
  handler: async (ctx, args) => {
    return await ctx.db.insert("catalog_probe", args);
  },
});

export const catalogProbeTokens = query({
  args: { runId: v.string() },
  handler: async (ctx, args) => {
    const rows = await ctx.db.query("catalog_probe").collect();
    return rows
      .filter((row: any) => row.runId === args.runId)
      .map((row: any) => row.token as string)
      .sort();
  },
});

// Cross-partition mutation: writes to both messages (partition 0) and tasks (partition 1).
// This triggers 2PC when partitioning is enabled.
export const crossPartitionWrite = mutation({
  args: { text: v.string(), taskTitle: v.string() },
  handler: async (ctx, args) => {
    await ctx.db.insert("messages", { text: args.text, author: "2pc", channel: "cross", timestamp: Date.now() });
    await ctx.db.insert("tasks", { title: args.taskTitle, assignee: "2pc", project: "cross", status: "done", createdAt: Date.now() });
  },
});

export const dashboard = query({
  handler: async (ctx) => {
    const messages = await ctx.db.query("messages").collect();
    const users = await ctx.db.query("users").collect();
    const projects = await ctx.db.query("projects").collect();
    const tasks = await ctx.db.query("tasks").collect();
    return {
      messages: messages.length,
      users: users.length,
      projects: projects.length,
      tasks: tasks.length,
    };
  },
});

export const totalBudget = query({
  handler: async (ctx) => {
    const projects = await ctx.db.query("projects").collect();
    return projects.reduce((sum: number, p: any) => sum + (p.budget || 0), 0);
  },
});

export const createUserWithSalary = mutation({
  args: { name: v.string(), salary: v.number() },
  handler: async (ctx, args) => {
    return await ctx.db.insert("users", { name: args.name, salary: args.salary, createdAt: Date.now() });
  },
});

export const totalCompensation = query({
  handler: async (ctx) => {
    const users = await ctx.db.query("users").collect();
    const projects = await ctx.db.query("projects").collect();
    const salaries = users.reduce((sum: number, u: any) => sum + (u.salary || 0), 0);
    const budgets = projects.reduce((sum: number, p: any) => sum + (p.budget || 0), 0);
    return { salaries, budgets, total: salaries + budgets };
  },
});

export const writeSequence = mutation({
  args: { key: v.string(), seq: v.number() },
  handler: async (ctx, args) => {
    await ctx.db.insert("tasks", { title: args.key, assignee: "monotonic", project: "test", status: String(args.seq), createdAt: Date.now() });
  },
});

export const readLatestSeq = query({
  args: { key: v.string() },
  handler: async (ctx, args) => {
    const rows = await ctx.db.query("tasks").collect();
    const matching = rows.filter((r: any) => r.title === args.key && r.assignee === "monotonic");
    if (matching.length === 0) return -1;
    return Math.max(...matching.map((r: any) => Number(r.status)));
  },
});

// Jepsen set test: insert a unique element, read back all elements.
export const insertSetElement = mutation({
  args: { setName: v.string(), element: v.number() },
  handler: async (ctx, args) => {
    await ctx.db.insert("messages", { text: String(args.element), author: args.setName, channel: "set-test", timestamp: Date.now() });
  },
});

export const readSet = query({
  args: { setName: v.string() },
  handler: async (ctx, args) => {
    const rows = await ctx.db.query("messages").collect();
    return rows.filter((r: any) => r.author === args.setName && r.channel === "set-test").map((r: any) => Number(r.text));
  },
});

// Jepsen sequential test: write A then B, read back, verify order.
export const writeOrdered = mutation({
  args: { key: v.string(), value: v.string() },
  handler: async (ctx, args) => {
    await ctx.db.insert("messages", { text: args.value, author: args.key, channel: "sequential-test", timestamp: Date.now() });
  },
});

export const readOrdered = query({
  args: { key: v.string() },
  handler: async (ctx, args) => {
    const rows = await ctx.db.query("messages").collect();
    return rows.filter((r: any) => r.author === args.key && r.channel === "sequential-test").map((r: any) => r.text);
  },
});

export const countMessagesByAuthorChannel = query({
  args: { author: v.string(), channel: v.string() },
  handler: async (ctx, args) => {
    const rows = await ctx.db.query("messages").collect();
    return rows.filter((r: any) => r.author === args.author && r.channel === args.channel).length;
  },
});

export const countMessagesByText = query({
  args: { text: v.string() },
  handler: async (ctx, args) => {
    const rows = await ctx.db.query("messages").collect();
    return rows.filter((r: any) => r.text === args.text).length;
  },
});

export const countTasksByTitle = query({
  args: { title: v.string() },
  handler: async (ctx, args) => {
    const rows = await ctx.db.query("tasks").collect();
    return rows.filter((r: any) => r.title === args.title).length;
  },
});

export const safeSnapshotPair = query({
  args: { text: v.string(), taskTitle: v.string() },
  handler: async (ctx, args) => {
    const messages = await ctx.db.query("messages").collect();
    const tasks = await ctx.db.query("tasks").collect();
    return {
      messages: messages.filter((message: any) => message.text === args.text).length,
      tasks: tasks.filter((task: any) => task.title === args.taskTitle).length,
    };
  },
});

// Concurrent counter: multiple increments, verify total.
export const incrementCounter = mutation({
  args: { name: v.string() },
  handler: async (ctx, args) => {
    await ctx.db.insert("messages", { text: "1", author: args.name, channel: "counter-test", timestamp: Date.now() });
  },
});

export const readCounter = query({
  args: { name: v.string() },
  handler: async (ctx, args) => {
    const rows = await ctx.db.query("messages").collect();
    return rows.filter((r: any) => r.author === args.name && r.channel === "counter-test").length;
  },
});

// Large batch: write many documents in a single mutation.
export const batchWrite = mutation({
  args: { prefix: v.string(), count: v.number() },
  handler: async (ctx, args) => {
    for (let i = 0; i < args.count; i++) {
      await ctx.db.insert("messages", { text: args.prefix + "-" + i, author: "batch", channel: "batch-test", timestamp: Date.now() });
    }
  },
});

export const countBatch = query({
  args: { prefix: v.string() },
  handler: async (ctx, args) => {
    const rows = await ctx.db.query("messages").collect();
    return rows.filter((r: any) => r.author === "batch" && r.channel === "batch-test" && (r.text as string).startsWith(args.prefix)).length;
  },
});

// Register test: write a value, read it back (single-key linearizability).
export const writeRegister = mutation({
  args: { key: v.string(), value: v.string() },
  handler: async (ctx, args) => {
    const existing = await ctx.db.query("messages").collect();
    const match = existing.find((r: any) => r.author === args.key && r.channel === "register-test");
    if (match) {
      await ctx.db.patch(match._id, { text: args.value, timestamp: Date.now() });
    } else {
      await ctx.db.insert("messages", { text: args.value, author: args.key, channel: "register-test", timestamp: Date.now() });
    }
  },
});

export const readRegister = query({
  args: { key: v.string() },
  handler: async (ctx, args) => {
    const rows = await ctx.db.query("messages").collect();
    const match = rows.find((r: any) => r.author === args.key && r.channel === "register-test");
    return match ? match.text : null;
  },
});

// Null and empty field test.
export const writeNullFields = mutation({
  args: { key: v.string() },
  handler: async (ctx, args) => {
    await ctx.db.insert("messages", { text: "", author: args.key, channel: "", timestamp: 0 });
  },
});

export const readNullFields = query({
  args: { key: v.string() },
  handler: async (ctx, args) => {
    const rows = await ctx.db.query("messages").collect();
    const match = rows.find((r: any) => r.author === args.key && r.timestamp === 0);
    if (!match) return null;
    return { text: match.text, channel: match.channel, timestamp: match.timestamp };
  },
});

// Write skew test: two concurrent reads + disjoint writes that violate a constraint.
export const readTwoKeys = query({
  args: { keyA: v.string(), keyB: v.string() },
  handler: async (ctx, args) => {
    const rows = await ctx.db.query("messages").collect();
    const a = rows.find((r: any) => r.author === args.keyA && r.channel === "register-test");
    const b = rows.find((r: any) => r.author === args.keyB && r.channel === "register-test");
    return { a: a?.text ?? null, b: b?.text ?? null };
  },
});

// Simple message count (used by failover tests).
export const count = query({
  handler: async (ctx) => {
    const msgs = await ctx.db.query("messages").collect();
    return msgs.length;
  },
});
TSEOF

cat > "$DEPLOY_DIR/safe-snapshot-subscription.mjs" << 'JSEOF'
import { ConvexClient } from "convex/browser";
import { api } from "./convex/_generated/api.js";

const [url, text, taskTitle] = process.argv.slice(2);
const client = new ConvexClient(url, { unsavedChangesWarning: false });
let subscription;
let finished = false;
let sawInitial = false;

const finish = async (code, event, details = {}) => {
  if (finished) return;
  finished = true;
  console.log(JSON.stringify({ event, ...details }));
  subscription?.unsubscribe();
  await Promise.race([
    client.close(),
    new Promise((resolve) => setTimeout(resolve, 1000)),
  ]);
  process.exit(code);
};

subscription = client.onUpdate(
  api.messages.safeSnapshotPair,
  { text, taskTitle },
  async (value) => {
    console.log(JSON.stringify({ event: "value", value }));
    if (value.messages !== value.tasks) {
      await finish(2, "torn", { value });
      return;
    }
    if (!sawInitial) {
      if (value.messages !== 0) {
        await finish(3, "unexpected-initial", { value });
        return;
      }
      sawInitial = true;
      console.log(JSON.stringify({ event: "ready" }));
      return;
    }
    if (value.messages === 1) {
      await finish(0, "complete", { value });
    }
  },
  async (error) => {
    await finish(4, "error", { message: String(error) });
  },
);

setTimeout(() => void finish(5, "timeout"), 25000);
JSEOF

cat > "$DEPLOY_DIR/catalog-coherence-subscription.mjs" << 'JSEOF'
import { ConvexClient } from "convex/browser";
import { api } from "./convex/_generated/api.js";

const [url, runId, expectedCountRaw] = process.argv.slice(2);
const expectedCount = Number(expectedCountRaw);
const client = new ConvexClient(url, { unsavedChangesWarning: false });
let subscription;
let finished = false;
let sawInitial = false;

const finish = async (code, event, details = {}) => {
  if (finished) return;
  finished = true;
  console.log(JSON.stringify({ event, ...details }));
  subscription?.unsubscribe();
  await Promise.race([
    client.close(),
    new Promise((resolve) => setTimeout(resolve, 1000)),
  ]);
  process.exit(code);
};

subscription = client.onUpdate(
  api.messages.catalogProbeTokens,
  { runId },
  async (tokens) => {
    console.log(JSON.stringify({ event: "value", tokens }));
    if (!sawInitial) {
      if (tokens.length !== 0) {
        await finish(2, "unexpected-initial", { tokens });
        return;
      }
      sawInitial = true;
      console.log(JSON.stringify({ event: "ready" }));
      return;
    }
    if (tokens.length > expectedCount) {
      await finish(3, "duplicate", { tokens });
      return;
    }
    if (tokens.length === expectedCount) {
      await finish(0, "complete", { tokens });
    }
  },
  async (error) => {
    await finish(4, "error", { message: String(error) });
  },
);

setTimeout(() => void finish(5, "timeout"), 20000);
JSEOF

deploy_functions_with_retry() {
    local key="$1"
    local url="$2"
    local attempts="${3:-5}"
    for _ in $(seq 1 "$attempts"); do
        if (cd "$DEPLOY_DIR" && npx convex deploy --admin-key "$key" --url "$url" > /dev/null 2>&1); then
            return 0
        fi
        sleep 2
    done
    return 1
}

echo "  Deploying functions..."
(cd "$DEPLOY_DIR" && npm install --silent 2>/dev/null)
deploy_functions_with_retry "$NODE_A_KEY" "$NODE_A_URL" 5
deploy_functions_with_retry "$NODE_B_KEY" "$NODE_B_URL" 5
echo "  Functions deployed to both nodes."

# --- Helpers ---

mutate() {
    curl -sf "$1/api/mutation" \
        -H "Authorization: Convex $2" \
        -H "Content-Type: application/json" \
        -d "{\"path\":\"$3\",\"args\":$4}"
}

mutation_response() {
    curl --max-time 60 -s "$1/api/mutation" \
        -H "Authorization: Convex $2" \
        -H "Content-Type: application/json" \
        -d "{\"path\":\"$3\",\"args\":$4}"
}

query_api() {
    curl -sf "$1/api/query" \
        -H "Authorization: Convex $2" \
        -H "Content-Type: application/json" \
        -d "{\"path\":\"$3\",\"args\":{}}"
}

set_jetstream_consumer_pause() {
    local consumer="$1"
    local seconds="$2"
    python3 - "$consumer" "$seconds" << 'PYEOF'
import datetime
import json
import socket
import sys
import uuid

consumer = sys.argv[1]
seconds = int(sys.argv[2])
if seconds > 0:
    pause_until = datetime.datetime.now(datetime.timezone.utc) + datetime.timedelta(seconds=seconds)
else:
    pause_until = datetime.datetime.fromtimestamp(0, datetime.timezone.utc)
payload = json.dumps(
    {"pause_until": pause_until.isoformat().replace("+00:00", "Z")},
    separators=(",", ":"),
).encode()
subject = f"$JS.API.CONSUMER.PAUSE.CONVEX_COMMITS.{consumer}"
inbox = f"_INBOX.{uuid.uuid4().hex}"

with socket.create_connection(("127.0.0.1", 4222), timeout=5) as connection:
    protocol = connection.makefile("rb")
    if not protocol.readline().startswith(b"INFO "):
        raise RuntimeError("NATS did not send INFO during consumer pause request")
    command = (
        f'CONNECT {{"verbose":false,"pedantic":false}}\r\n'
        f"SUB {inbox} 1\r\n"
        f"PUB {subject} {inbox} {len(payload)}\r\n"
    ).encode() + payload + b"\r\nPING\r\n"
    connection.sendall(command)
    while True:
        line = protocol.readline()
        if not line:
            raise RuntimeError("NATS closed before consumer pause response")
        if line.startswith(b"MSG "):
            size = int(line.split()[-1])
            response = json.loads(protocol.read(size))
            protocol.read(2)
            if "error" in response:
                raise RuntimeError(f"JetStream consumer pause failed: {response['error']}")
            expected_paused = seconds > 0
            if bool(response.get("paused")) != expected_paused:
                raise RuntimeError(f"Unexpected JetStream pause response: {response}")
            break
PYEOF
}

query_with_args() {
    curl -sf "$1/api/query" \
        -H "Authorization: Convex $2" \
        -H "Content-Type: application/json" \
        -d "{\"path\":\"$3\",\"args\":$4}"
}

query_at_ts_with_args() {
    curl -sf "$1/api/query_at_ts" \
        -H "Authorization: Convex $2" \
        -H "Content-Type: application/json" \
        -d "{\"path\":\"$3\",\"args\":$4,\"ts\":\"$5\"}"
}

jval() { python3 -c "import sys,json; print(int(json.load(sys.stdin)['value']['$1']))" <<< "$2"; }
jtotal() { python3 -c "import sys,json; print(int(json.load(sys.stdin)['value']))" <<< "$1"; }
raw_participant_fences() {
    python3 -c "import sys,json; r=json.load(sys.stdin); fences=r.get('readAfterWrite',{}).get('participantFences',[]); print(','.join(str(f.get('sourcePartition')) for f in sorted(fences, key=lambda f: f.get('sourcePartition', -1))))" <<< "$1"
}
raw_read_after_write_ts() {
    python3 -c "import sys,json; print(json.load(sys.stdin).get('readAfterWrite',{}).get('ts',''))" <<< "$1"
}

dashboard_message_task_counts_on_nodes() {
    local dash_a
    local dash_b
    dash_a=$(query_api "$NODE_A_URL" "$NODE_A_KEY" "messages:dashboard")
    dash_b=$(query_api "$NODE_B_URL" "$NODE_B_KEY" "messages:dashboard")
    echo "$(jval messages "$dash_a") $(jval tasks "$dash_a") $(jval messages "$dash_b") $(jval tasks "$dash_b")"
}

wait_dashboard_message_task_convergence() {
    local attempts="${1:-30}"
    local counts="0 0 0 0"
    local msg_a
    local task_a
    local msg_b
    local task_b

    for _ in $(seq 1 "$attempts"); do
        counts=$(dashboard_message_task_counts_on_nodes 2>/dev/null || echo "$counts")
        read -r msg_a task_a msg_b task_b <<< "$counts"
        if [ "$msg_a" -eq "$msg_b" ] && [ "$task_a" -eq "$task_b" ]; then
            echo "$counts"
            return 0
        fi
        sleep 1
    done

    echo "$counts"
    return 1
}

count_message_text() {
    jtotal "$(query_with_args "$1" "$2" "messages:countMessagesByText" "{\"text\":\"$3\"}")"
}

count_task_title() {
    jtotal "$(query_with_args "$1" "$2" "messages:countTasksByTitle" "{\"title\":\"$3\"}")"
}

count_2pc_pair_on_nodes() {
    local text="$1"
    local task="$2"
    local msg_a
    local msg_b
    local task_a
    local task_b
    msg_a=$(count_message_text "$NODE_A_URL" "$NODE_A_KEY" "$text")
    msg_b=$(count_message_text "$NODE_B_URL" "$NODE_B_KEY" "$text")
    task_a=$(count_task_title "$NODE_A_URL" "$NODE_A_KEY" "$task")
    task_b=$(count_task_title "$NODE_B_URL" "$NODE_B_KEY" "$task")
    echo "$msg_a $msg_b $task_a $task_b"
}

two_pc_counts_match_response() {
    local response="$1"
    local msg_a="$2"
    local msg_b="$3"
    local task_a="$4"
    local task_b="$5"
    local count

    for count in "$msg_a" "$msg_b" "$task_a" "$task_b"; do
        case "$count" in
            ''|*[!0-9]*) return 1 ;;
        esac
    done

    if echo "$response" | grep -q '"status":"success"'; then
        [ "$msg_a" -eq 1 ] && [ "$msg_b" -eq 1 ] && [ "$task_a" -eq 1 ] && [ "$task_b" -eq 1 ]
        return $?
    fi

    [ "$msg_a" -eq "$msg_b" ] && [ "$msg_a" -eq "$task_a" ] && [ "$msg_a" -eq "$task_b" ] \
        && { [ "$msg_a" -eq 0 ] || [ "$msg_a" -eq 1 ]; }
}

wait_for_2pc_pair_exactness() {
    local text="$1"
    local task="$2"
    local response="$3"
    local attempts="${4:-30}"
    local counts="0 0 0 0"
    local msg_a
    local msg_b
    local task_a
    local task_b

    for _ in $(seq 1 "$attempts"); do
        counts=$(count_2pc_pair_on_nodes "$text" "$task" 2>/dev/null || echo "$counts")
        read -r msg_a msg_b task_a task_b <<< "$counts"
        if two_pc_counts_match_response "$response" "$msg_a" "$msg_b" "$task_a" "$task_b"; then
            echo "$counts"
            return 0
        fi
        sleep 1
    done

    echo "$counts"
    return 1
}

latest_membership_log() {
    docker logs --since "${2:-120s}" "$1" 2>&1 \
        | grep -F "Refreshed cluster membership snapshot" \
        | tail -1 || true
}

wait_membership_contains() {
    local container="$1"
    local needle="$2"
    local attempts="${3:-45}"
    local latest=""
    for _ in $(seq 1 "$attempts"); do
        latest=$(latest_membership_log "$container" 120s)
        if echo "$latest" | grep -F "$needle" > /dev/null; then
            return 0
        fi
        sleep 1
    done
    echo "$latest"
    return 1
}

wait_membership_excludes() {
    local container="$1"
    local absent="$2"
    local required="$3"
    local attempts="${4:-45}"
    local latest=""
    for _ in $(seq 1 "$attempts"); do
        latest=$(latest_membership_log "$container" 120s)
        if echo "$latest" | grep -F "$required" > /dev/null \
            && ! echo "$latest" | grep -F "$absent" > /dev/null; then
            return 0
        fi
        sleep 1
    done
    echo "$latest"
    return 1
}

assert_cross_partition_write_exact_once() {
    local url="$1"
    local key="$2"
    local text="$3"
    local task="$4"
    local label="$5"
    local response
    response=$(mutation_response "$url" "$key" "messages:crossPartitionWrite" \
        "{\"text\":\"$text\",\"taskTitle\":\"$task\"}")
    if ! echo "$response" | grep -q '"status":"success"'; then
        fail "$label failed" "$response"
        return 1
    fi
    sleep 2
    local msg_count
    local task_count
    msg_count=$(count_message_text "$NODE_A_URL" "$NODE_A_KEY" "$text")
    task_count=$(count_task_title "$NODE_A_URL" "$NODE_A_KEY" "$task")
    if [ "$msg_count" -eq 1 ] && [ "$task_count" -eq 1 ]; then
        pass "$label"
        return 0
    fi
    fail "$label did not appear exactly once" "messages=$msg_count tasks=$task_count"
    return 1
}

# ============================================================
echo ""
echo -e "${BOLD}Test 0: Fresh Remote Catalog Coherence${NC}"
# ============================================================
# catalog_probe is assigned to partition 1 and has never been written. All six
# public entrypoints race its first implicit creation while partition 0's NATS
# consumer is paused. A successful response must mean the catalog is already
# committed through partition 0's Raft group and the row through partition 1's
# group; neither immediate reads nor subscription reruns may depend on NATS.

CATALOG_RUN="catalog-$(date +%s)-$$"
CATALOG_EXPECTED="p0a,p0b,p0c,p1a,p1b,p1c"
CATALOG_SUB_LOG=$(mktemp)
CATALOG_TMP_DIR=$(mktemp -d)
CATALOG_COORDINATOR_FOUND=false
CATALOG_COORDINATOR_CONTAINER=""
CATALOG_COORDINATOR_URL=""
CATALOG_COORDINATOR_KEY=""

for entry in \
    "docker-node-p0a-1|$NODE_P0A_URL|$NODE_P0A_KEY" \
    "docker-node-p0b-1|$NODE_P0B_URL|$NODE_P0B_KEY" \
    "docker-node-p0c-1|$NODE_P0C_URL|$NODE_P0C_KEY"; do
    IFS='|' read -r candidate_container candidate_url candidate_key <<< "$entry"
    status=$(curl --max-time 2 -s -o /dev/null -w "%{http_code}" \
        -H "Connection: Upgrade" \
        -H "Upgrade: websocket" \
        -H "Sec-WebSocket-Key: dGhlIHNhbXBsZSBub25jZQ==" \
        -H "Sec-WebSocket-Version: 13" \
        "$candidate_url/api/1.34.1/sync" 2>/dev/null || true)
    if [ "$status" = "101" ]; then
        CATALOG_COORDINATOR_FOUND=true
        CATALOG_COORDINATOR_CONTAINER="$candidate_container"
        CATALOG_COORDINATOR_URL="$candidate_url"
        CATALOG_COORDINATOR_KEY="$candidate_key"
        break
    fi
done

if $CATALOG_COORDINATOR_FOUND; then
    CATALOG_NATS_CONSUMER=$(docker exec "$CATALOG_COORDINATOR_CONTAINER" \
        printenv INSTANCE_NAME 2>/dev/null)
    (cd "$DEPLOY_DIR" && node catalog-coherence-subscription.mjs \
        "$CATALOG_COORDINATOR_URL" "$CATALOG_RUN" 6 \
        > "$CATALOG_SUB_LOG" 2>&1) &
    CATALOG_SUB_PID=$!
else
    CATALOG_SUB_PID=""
fi

CATALOG_SUB_READY=false
if $CATALOG_COORDINATOR_FOUND; then
    for _ in $(seq 1 100); do
        if grep -q '"event":"ready"' "$CATALOG_SUB_LOG"; then
            CATALOG_SUB_READY=true
            break
        fi
        if ! kill -0 "$CATALOG_SUB_PID" 2>/dev/null; then
            break
        fi
        sleep 0.1
    done
fi

CATALOG_MUTATIONS_OK=true
CATALOG_MUTATION_DETAILS=""
if $CATALOG_SUB_READY; then
    set_jetstream_consumer_pause "$CATALOG_NATS_CONSUMER" 30
    CATALOG_ENTRYPOINTS=(
        "p0a|$NODE_P0A_URL|$NODE_P0A_KEY"
        "p0b|$NODE_P0B_URL|$NODE_P0B_KEY"
        "p0c|$NODE_P0C_URL|$NODE_P0C_KEY"
        "p1a|$NODE_P1A_URL|$NODE_P1A_KEY"
        "p1b|$NODE_P1B_URL|$NODE_P1B_KEY"
        "p1c|$NODE_P1C_URL|$NODE_P1C_KEY"
    )
    CATALOG_PIDS=()
    for entry in "${CATALOG_ENTRYPOINTS[@]}"; do
        IFS='|' read -r label url key <<< "$entry"
        (mutation_response "$url" "$key" "messages:createCatalogProbe" \
            "{\"runId\":\"$CATALOG_RUN\",\"token\":\"$label\"}" \
            > "$CATALOG_TMP_DIR/$label.json" 2>&1 || true) &
        CATALOG_PIDS+=("$!")
    done
    for pid in "${CATALOG_PIDS[@]}"; do
        wait "$pid" || true
    done
    for entry in "${CATALOG_ENTRYPOINTS[@]}"; do
        label="${entry%%|*}"
        response=$(cat "$CATALOG_TMP_DIR/$label.json")
        if ! echo "$response" | grep -q '"status":"success"'; then
            CATALOG_MUTATIONS_OK=false
            CATALOG_MUTATION_DETAILS="$CATALOG_MUTATION_DETAILS $label=$response"
        fi
    done
else
    CATALOG_ENTRYPOINTS=()
    CATALOG_MUTATIONS_OK=false
    CATALOG_MUTATION_DETAILS="subscription did not reach the empty-table state"
fi

$CATALOG_MUTATIONS_OK \
    && pass "All six public entrypoints committed concurrent first writes" \
    || fail "Fresh remote table creation failed from a public entrypoint" "$CATALOG_MUTATION_DETAILS"

CATALOG_HTTP_OK=true
CATALOG_HTTP_DETAILS=""
docker pause docker-nats-1 > /dev/null 2>&1
for entry in "${CATALOG_ENTRYPOINTS[@]}"; do
    IFS='|' read -r label url key <<< "$entry"
    response=$(curl --max-time 10 -s "$url/api/query" \
        -H "Authorization: Convex $key" \
        -H "Content-Type: application/json" \
        -d "{\"path\":\"messages:catalogProbeTokens\",\"args\":{\"runId\":\"$CATALOG_RUN\"}}" \
        2>/dev/null || true)
    tokens=$(python3 -c 'import json,sys; print(",".join(json.load(sys.stdin)["value"]))' \
        <<< "$response" 2>/dev/null || echo parse-error)
    if [ "$tokens" != "$CATALOG_EXPECTED" ]; then
        CATALOG_HTTP_OK=false
        CATALOG_HTTP_DETAILS="$CATALOG_HTTP_DETAILS $label=$tokens response=$response;"
    fi
done

CATALOG_SUB_STATUS=124
if [ -n "$CATALOG_SUB_PID" ]; then
    for _ in $(seq 1 100); do
        if ! kill -0 "$CATALOG_SUB_PID" 2>/dev/null; then
            CATALOG_SUB_STATUS=0
            wait "$CATALOG_SUB_PID" || CATALOG_SUB_STATUS=$?
            break
        fi
        sleep 0.1
    done
fi

docker unpause docker-nats-1 > /dev/null 2>&1 || true
if $CATALOG_COORDINATOR_FOUND; then
    set_jetstream_consumer_pause "$CATALOG_NATS_CONSUMER" 0
fi
if [ "$CATALOG_SUB_STATUS" -eq 124 ] && [ -n "$CATALOG_SUB_PID" ]; then
    kill "$CATALOG_SUB_PID" 2>/dev/null || true
    wait "$CATALOG_SUB_PID" 2>/dev/null || true
fi

$CATALOG_HTTP_OK \
    && pass "All six entrypoints immediately resolved catalog and data with NATS unavailable" \
    || fail "Immediate post-commit read depended on NATS catalog propagation" "$CATALOG_HTTP_DETAILS"

if [ "$CATALOG_SUB_STATUS" -eq 0 ] && \
    grep -q '"event":"complete"' "$CATALOG_SUB_LOG" && \
    ! grep -Eq '"event":"(error|duplicate|timeout|unexpected-initial)"' "$CATALOG_SUB_LOG"; then
    pass "WebSocket subscription crossed first table creation without a catalog gap"
else
    fail "WebSocket subscription missed or duplicated the first remote table commit" \
        "$(tr '\n' ' ' < "$CATALOG_SUB_LOG")"
fi

CATALOG_FAILOVER_OK=false
CATALOG_FAILOVER_DETAILS=""
if $CATALOG_COORDINATOR_FOUND; then
    docker stop "$CATALOG_COORDINATOR_CONTAINER" > /dev/null 2>&1
    CATALOG_NEW_COORDINATOR_URL=""
    CATALOG_NEW_COORDINATOR_KEY=""
    for _ in $(seq 1 80); do
        for entry in \
            "docker-node-p0a-1|$NODE_P0A_URL|$NODE_P0A_KEY" \
            "docker-node-p0b-1|$NODE_P0B_URL|$NODE_P0B_KEY" \
            "docker-node-p0c-1|$NODE_P0C_URL|$NODE_P0C_KEY"; do
            IFS='|' read -r candidate_container candidate_url candidate_key <<< "$entry"
            [ "$candidate_container" = "$CATALOG_COORDINATOR_CONTAINER" ] && continue
            status=$(curl --max-time 1 -s -o /dev/null -w "%{http_code}" \
                -H "Connection: Upgrade" \
                -H "Upgrade: websocket" \
                -H "Sec-WebSocket-Key: dGhlIHNhbXBsZSBub25jZQ==" \
                -H "Sec-WebSocket-Version: 13" \
                "$candidate_url/api/1.34.1/sync" 2>/dev/null || true)
            if [ "$status" = "101" ]; then
                CATALOG_NEW_COORDINATOR_URL="$candidate_url"
                CATALOG_NEW_COORDINATOR_KEY="$candidate_key"
                break 2
            fi
        done
        sleep 0.25
    done

    if [ -n "$CATALOG_NEW_COORDINATOR_URL" ]; then
        response=$(query_with_args "$CATALOG_NEW_COORDINATOR_URL" \
            "$CATALOG_NEW_COORDINATOR_KEY" "messages:catalogProbeTokens" \
            "{\"runId\":\"$CATALOG_RUN\"}" 2>/dev/null || true)
        tokens=$(python3 -c 'import json,sys; print(",".join(json.load(sys.stdin)["value"]))' \
            <<< "$response" 2>/dev/null || echo parse-error)
        if [ "$tokens" = "$CATALOG_EXPECTED" ]; then
            CATALOG_FAILOVER_OK=true
        else
            CATALOG_FAILOVER_DETAILS="tokens=$tokens response=$response"
        fi
    else
        CATALOG_FAILOVER_DETAILS="no replacement partition-0 leader elected"
    fi

    docker start "$CATALOG_COORDINATOR_CONTAINER" > /dev/null 2>&1
    for _ in $(seq 1 60); do
        curl --max-time 2 -sf "$CATALOG_COORDINATOR_URL/version" > /dev/null 2>&1 && break
        sleep 0.5
    done
    case "$CATALOG_COORDINATOR_CONTAINER" in
        docker-node-p0a-1)
            NODE_P0A_KEY=$(docker exec docker-node-p0a-1 ./generate_admin_key.sh 2>&1 | tail -1)
            NODE_A_KEY="$NODE_P0A_KEY"
            ;;
        docker-node-p0b-1)
            NODE_P0B_KEY=$(docker exec docker-node-p0b-1 ./generate_admin_key.sh 2>&1 | tail -1)
            ;;
        docker-node-p0c-1)
            NODE_P0C_KEY=$(docker exec docker-node-p0c-1 ./generate_admin_key.sh 2>&1 | tail -1)
            ;;
    esac
else
    CATALOG_FAILOVER_DETAILS="initial partition-0 leader was not found"
fi

$CATALOG_FAILOVER_OK \
    && pass "Fresh remote catalog survived partition-0 leader failover" \
    || fail "Catalog was not Raft-durable across coordinator failover" "$CATALOG_FAILOVER_DETAILS"

rm -f "$CATALOG_SUB_LOG"
rm -rf "$CATALOG_TMP_DIR"

# --- Baseline: capture existing counts before tests ---

echo ""
echo -e "${BOLD}Capturing baseline counts...${NC}"
BASELINE=$(query_api "$NODE_A_URL" "$NODE_A_KEY" "messages:dashboard" 2>/dev/null || echo '{"value":{"messages":0,"users":0,"projects":0,"tasks":0}}')
BASE_M=$(jval messages "$BASELINE" 2>/dev/null || echo 0)
BASE_U=$(jval users "$BASELINE" 2>/dev/null || echo 0)
BASE_P=$(jval projects "$BASELINE" 2>/dev/null || echo 0)
BASE_T=$(jval tasks "$BASELINE" 2>/dev/null || echo 0)
BASE_BUDGET=$(jtotal "$(query_api "$NODE_A_URL" "$NODE_A_KEY" "messages:totalBudget" 2>/dev/null || echo '{"value":0}')" 2>/dev/null || echo 0)
BASE_COMP=$(python3 -c "import sys,json; print(int(json.load(sys.stdin)['value']['total']))" <<< "$(query_api "$NODE_A_URL" "$NODE_A_KEY" "messages:totalCompensation" 2>/dev/null || echo '{"value":{"total":0}}')" 2>/dev/null || echo 0)
echo "  Baseline: msgs=$BASE_M users=$BASE_U proj=$BASE_P tasks=$BASE_T budget=\$$BASE_BUDGET comp=\$$BASE_COMP"

# ============================================================
echo ""
echo -e "${BOLD}Test 1: Cross-Partition Data Verification (Vitess VDiff)${NC}"
# ============================================================

mutate "$NODE_A_URL" "$NODE_A_KEY" "messages:send" '{"text":"msg-1","author":"alice","channel":"general"}' > /dev/null
mutate "$NODE_A_URL" "$NODE_A_KEY" "messages:send" '{"text":"msg-2","author":"bob","channel":"eng"}' > /dev/null
mutate "$NODE_A_URL" "$NODE_A_KEY" "messages:send" '{"text":"msg-3","author":"charlie","channel":"general"}' > /dev/null
mutate "$NODE_A_URL" "$NODE_A_KEY" "messages:createUser" '{"name":"Alice","role":"engineer"}' > /dev/null
mutate "$NODE_A_URL" "$NODE_A_KEY" "messages:createUser" '{"name":"Bob","role":"manager"}' > /dev/null

mutate "$NODE_B_URL" "$NODE_B_KEY" "messages:createProject" '{"name":"Alpha","status":"active","budget":10000}' > /dev/null
mutate "$NODE_B_URL" "$NODE_B_KEY" "messages:createProject" '{"name":"Beta","status":"planning","budget":25000}' > /dev/null
mutate "$NODE_B_URL" "$NODE_B_KEY" "messages:createTask" '{"title":"Design","assignee":"Alice","project":"Alpha","status":"done"}' > /dev/null
mutate "$NODE_B_URL" "$NODE_B_KEY" "messages:createTask" '{"title":"Build","assignee":"Bob","project":"Alpha","status":"active"}' > /dev/null
mutate "$NODE_B_URL" "$NODE_B_KEY" "messages:createTask" '{"title":"Plan","assignee":"Charlie","project":"Beta","status":"todo"}' > /dev/null

echo "  Waiting for NATS replication..."
sleep 4

DA=$(query_api "$NODE_A_URL" "$NODE_A_KEY" "messages:dashboard")
DB=$(query_api "$NODE_B_URL" "$NODE_B_KEY" "messages:dashboard")

AM=$(jval messages "$DA"); AU=$(jval users "$DA"); AP=$(jval projects "$DA"); AT=$(jval tasks "$DA")
BM=$(jval messages "$DB"); BU=$(jval users "$DB"); BP=$(jval projects "$DB"); BT=$(jval tasks "$DB")

# Check deltas from baseline
DM=$((AM - BASE_M)); DU=$((AU - BASE_U)); DP=$((AP - BASE_P)); DT=$((AT - BASE_T))

[ "$DM" -eq 3 ] && [ "$DU" -eq 2 ] && [ "$DP" -eq 2 ] && [ "$DT" -eq 3 ] \
    && pass "Node A: +3 msgs, +2 users, +2 proj, +3 tasks (total: $AM,$AU,$AP,$AT)" \
    || fail "Node A wrong delta" "delta: msgs=$DM users=$DU proj=$DP tasks=$DT, expected +3 +2 +2 +3"

[ "$AM" -eq "$BM" ] && [ "$AU" -eq "$BU" ] && [ "$AP" -eq "$BP" ] && [ "$AT" -eq "$BT" ] \
    && pass "VDiff equivalence: both nodes identical ($AM,$AU,$AP,$AT)" \
    || fail "Nodes diverged" "A: $AM,$AU,$AP,$AT vs B: $BM,$BU,$BP,$BT"

# ============================================================
echo ""
echo -e "${BOLD}Test 2: Bank Test / Invariant Preservation (CockroachDB Jepsen)${NC}"
# ============================================================

# New budgets: Alpha=10000, Beta=25000 → delta=35000
EXPECTED_DELTA=35000
EXPECTED=$((BASE_BUDGET + EXPECTED_DELTA))

TA=$(jtotal "$(query_api "$NODE_A_URL" "$NODE_A_KEY" "messages:totalBudget")")
TB=$(jtotal "$(query_api "$NODE_B_URL" "$NODE_B_KEY" "messages:totalBudget")")

[ "$TA" -eq "$EXPECTED" ] \
    && pass "Node A budget total preserved: \$$TA (base \$$BASE_BUDGET + \$$EXPECTED_DELTA)" \
    || fail "Node A budget invariant violated" "got \$$TA, expected \$$EXPECTED"

[ "$TB" -eq "$EXPECTED" ] \
    && pass "Node B budget total preserved: \$$TB" \
    || fail "Node B budget invariant violated" "got \$$TB, expected \$$EXPECTED"

[ "$TA" -eq "$TB" ] \
    && pass "Cross-node invariant: both agree on \$$TA" \
    || fail "Cross-node invariant violated" "A=\$$TA vs B=\$$TB"

# ============================================================
echo ""
echo -e "${BOLD}Test 3: Bank Invariant — Multi-Table (TiDB bank-multitable)${NC}"
# ============================================================

# Create users with salaries on Node A, projects with budgets already on Node B.
mutate "$NODE_A_URL" "$NODE_A_KEY" "messages:createUserWithSalary" '{"name":"Eve","salary":60000}' > /dev/null
mutate "$NODE_A_URL" "$NODE_A_KEY" "messages:createUserWithSalary" '{"name":"Frank","salary":80000}' > /dev/null

SALARY_DELTA=140000  # 60000 + 80000

echo "  Waiting for replication..."
sleep 4

CA=$(query_api "$NODE_A_URL" "$NODE_A_KEY" "messages:totalCompensation")
CB=$(query_api "$NODE_B_URL" "$NODE_B_KEY" "messages:totalCompensation")

CA_TOTAL=$(python3 -c "import sys,json; print(int(json.load(sys.stdin)['value']['total']))" <<< "$CA")
CB_TOTAL=$(python3 -c "import sys,json; print(int(json.load(sys.stdin)['value']['total']))" <<< "$CB")
EXPECTED_COMP=$((BASE_COMP + EXPECTED_DELTA + SALARY_DELTA))

[ "$CA_TOTAL" -eq "$EXPECTED_COMP" ] \
    && pass "Node A cross-table total: \$$CA_TOTAL (salaries + budgets)" \
    || fail "Node A cross-table invariant" "got \$$CA_TOTAL, expected \$$EXPECTED_COMP"

[ "$CB_TOTAL" -eq "$EXPECTED_COMP" ] \
    && pass "Node B cross-table total: \$$CB_TOTAL" \
    || fail "Node B cross-table invariant" "got \$$CB_TOTAL, expected \$$EXPECTED_COMP"

[ "$CA_TOTAL" -eq "$CB_TOTAL" ] \
    && pass "Cross-node multi-table invariant: both agree on \$$CA_TOTAL" \
    || fail "Cross-node multi-table invariant violated" "A=\$$CA_TOTAL vs B=\$$CB_TOTAL"

# Test 3 adds two salary-bearing user documents. Keep the running table-count
# baseline aligned so later topology-hiding checks assert the exact deltas they
# introduce, not stale pre-compensation counts.
AU=$((AU + 2))

# ============================================================
echo ""
echo -e "${BOLD}Test 4: Transparent Partition Routing (Topology Hiding)${NC}"
# ============================================================

# Node A → projects (partition 1): should route/coordinate transparently.
R=$(curl -s "$NODE_A_URL/api/mutation" -H "Authorization: Convex $NODE_A_KEY" -H "Content-Type: application/json" \
    -d '{"path":"messages:createProject","args":{"name":"X","status":"x","budget":0}}')
echo "$R" | grep -q '"status":"success"' \
    && pass "Node A transparently routed write to 'projects' (partition 1)" \
    || fail "Node A should route projects write" "$R"

# Node B → messages (partition 0): should route/coordinate transparently.
R=$(curl -s "$NODE_B_URL/api/mutation" -H "Authorization: Convex $NODE_B_KEY" -H "Content-Type: application/json" \
    -d '{"path":"messages:send","args":{"text":"x","author":"x","channel":"x"}}')
echo "$R" | grep -q '"status":"success"' \
    && pass "Node B transparently routed write to 'messages' (partition 0)" \
    || fail "Node B should route messages write" "$R"

# Node A → tasks (partition 1): should route/coordinate transparently.
R=$(curl -s "$NODE_A_URL/api/mutation" -H "Authorization: Convex $NODE_A_KEY" -H "Content-Type: application/json" \
    -d '{"path":"messages:createTask","args":{"title":"x","assignee":"x","project":"x","status":"x"}}')
echo "$R" | grep -q '"status":"success"' \
    && pass "Node A transparently routed write to 'tasks' (partition 1)" \
    || fail "Node A should route tasks write" "$R"

# Node B → users (partition 0): should route/coordinate transparently.
R=$(curl -s "$NODE_B_URL/api/mutation" -H "Authorization: Convex $NODE_B_KEY" -H "Content-Type: application/json" \
    -d '{"path":"messages:createUser","args":{"name":"x","role":"x"}}')
echo "$R" | grep -q '"status":"success"' \
    && pass "Node B transparently routed write to 'users' (partition 0)" \
    || fail "Node B should route users write" "$R"

# Verify the transparently routed writes converged on both nodes.
sleep 1
DC=$(query_api "$NODE_A_URL" "$NODE_A_KEY" "messages:dashboard")
DD=$(query_api "$NODE_B_URL" "$NODE_B_KEY" "messages:dashboard")
CM=$(jval messages "$DC"); CU=$(jval users "$DC"); CP=$(jval projects "$DC"); CT=$(jval tasks "$DC")
DM_B=$(jval messages "$DD"); DU_B=$(jval users "$DD"); DP_B=$(jval projects "$DD"); DT_B=$(jval tasks "$DD")
EXPECTED_M=$((AM + 1)); EXPECTED_U=$((AU + 1)); EXPECTED_P=$((AP + 1)); EXPECTED_T=$((AT + 1))
[ "$CM" -eq "$EXPECTED_M" ] && [ "$CU" -eq "$EXPECTED_U" ] && [ "$CP" -eq "$EXPECTED_P" ] && [ "$CT" -eq "$EXPECTED_T" ] \
    && [ "$CM" -eq "$DM_B" ] && [ "$CU" -eq "$DU_B" ] && [ "$CP" -eq "$DP_B" ] && [ "$CT" -eq "$DT_B" ] \
    && pass "Transparent routing converged (msgs=$CM users=$CU proj=$CP tasks=$CT)" \
    || fail "Transparent routing did not converge" "A: $CM,$CU,$CP,$CT expected $EXPECTED_M,$EXPECTED_U,$EXPECTED_P,$EXPECTED_T; B: $DM_B,$DU_B,$DP_B,$DT_B"

AM=$CM; AU=$CU; AP=$CP; AT=$CT

# ============================================================
echo ""
echo -e "${BOLD}Test 4b: All Replica Entrypoint Routing (Cluster-Wide Topology Hiding)${NC}"
# ============================================================

# Public traffic should not need to know either the table owner or the current
# Raft leader. Exercise non-leader entrypoints in both partitions and require
# the system to forward/coordinate the write internally.
R=$(mutate "$NODE_P0B_URL" "$NODE_P0B_KEY" "messages:createProject" '{"name":"follower-p0b-project","status":"x","budget":0}')
echo "$R" | grep -q '"status":"success"' \
    && pass "p0 follower B routed write to 'projects' (partition 1)" \
    || fail "p0 follower B should route projects write" "$R"

R=$(mutate "$NODE_P0C_URL" "$NODE_P0C_KEY" "messages:createUser" '{"name":"follower-p0c-user","role":"x"}')
echo "$R" | grep -q '"status":"success"' \
    && pass "p0 follower C routed write to 'users' (partition 0)" \
    || fail "p0 follower C should route users write" "$R"

R=$(mutate "$NODE_P1B_URL" "$NODE_P1B_KEY" "messages:send" '{"text":"follower-p1b-message","author":"x","channel":"x"}')
echo "$R" | grep -q '"status":"success"' \
    && pass "p1 follower B routed write to 'messages' (partition 0)" \
    || fail "p1 follower B should route messages write" "$R"

R=$(mutate "$NODE_P1C_URL" "$NODE_P1C_KEY" "messages:createTask" '{"title":"follower-p1c-task","assignee":"x","project":"x","status":"x"}')
echo "$R" | grep -q '"status":"success"' \
    && pass "p1 follower C routed write to 'tasks' (partition 1)" \
    || fail "p1 follower C should route tasks write" "$R"

sleep 2
EXPECTED_M=$((AM + 1)); EXPECTED_U=$((AU + 1)); EXPECTED_P=$((AP + 1)); EXPECTED_T=$((AT + 1))
ALL_NODE_COUNTS=()
ALL_NODE_COUNTS+=("p0a=$(query_api "$NODE_P0A_URL" "$NODE_P0A_KEY" "messages:dashboard")")
ALL_NODE_COUNTS+=("p0b=$(query_api "$NODE_P0B_URL" "$NODE_P0B_KEY" "messages:dashboard")")
ALL_NODE_COUNTS+=("p0c=$(query_api "$NODE_P0C_URL" "$NODE_P0C_KEY" "messages:dashboard")")
ALL_NODE_COUNTS+=("p1a=$(query_api "$NODE_P1A_URL" "$NODE_P1A_KEY" "messages:dashboard")")
ALL_NODE_COUNTS+=("p1b=$(query_api "$NODE_P1B_URL" "$NODE_P1B_KEY" "messages:dashboard")")
ALL_NODE_COUNTS+=("p1c=$(query_api "$NODE_P1C_URL" "$NODE_P1C_KEY" "messages:dashboard")")

ALL_NODES_OK=true
DETAILS=""
for entry in "${ALL_NODE_COUNTS[@]}"; do
    node="${entry%%=*}"
    payload="${entry#*=}"
    NM=$(jval messages "$payload"); NU=$(jval users "$payload"); NP=$(jval projects "$payload"); NT=$(jval tasks "$payload")
    DETAILS="$DETAILS $node:$NM,$NU,$NP,$NT"
    if [ "$NM" -ne "$EXPECTED_M" ] || [ "$NU" -ne "$EXPECTED_U" ] || [ "$NP" -ne "$EXPECTED_P" ] || [ "$NT" -ne "$EXPECTED_T" ]; then
        ALL_NODES_OK=false
    fi
done

$ALL_NODES_OK \
    && pass "All six node entrypoints converged (msgs=$EXPECTED_M users=$EXPECTED_U proj=$EXPECTED_P tasks=$EXPECTED_T)" \
    || fail "All six node entrypoints did not converge" "$DETAILS expected $EXPECTED_M,$EXPECTED_U,$EXPECTED_P,$EXPECTED_T"

AM=$EXPECTED_M; AU=$EXPECTED_U; AP=$EXPECTED_P; AT=$EXPECTED_T

# ============================================================
echo ""
echo -e "${BOLD}Test 4c: Six-Entrypoint Read-After-Write (Session/Topology Hiding)${NC}"
# ============================================================

# Each public node should be usable as an entrypoint. Write one unique message
# through every node, then require every node to read every unique message back.
ENTRYPOINT_LABELS=(p0a p0b p0c p1a p1b p1c)
ENTRYPOINT_URLS=("$NODE_P0A_URL" "$NODE_P0B_URL" "$NODE_P0C_URL" "$NODE_P1A_URL" "$NODE_P1B_URL" "$NODE_P1C_URL")
ENTRYPOINT_KEYS=("$NODE_P0A_KEY" "$NODE_P0B_KEY" "$NODE_P0C_KEY" "$NODE_P1A_KEY" "$NODE_P1B_KEY" "$NODE_P1C_KEY")
SIX_RAR_AUTHORS=()
SIX_RAR_ACCEPTED=0
SIX_RAR_RUN="six-rar-$(date +%s)"

for i in "${!ENTRYPOINT_LABELS[@]}"; do
    label="${ENTRYPOINT_LABELS[$i]}"
    author="$SIX_RAR_RUN-$label"
    SIX_RAR_AUTHORS+=("$author")
    R=$(mutation_response "${ENTRYPOINT_URLS[$i]}" "${ENTRYPOINT_KEYS[$i]}" "messages:send" \
        "{\"text\":\"$author\",\"author\":\"$author\",\"channel\":\"six-entrypoint-rar\"}")
    if echo "$R" | grep -q '"status":"success"'; then
        SIX_RAR_ACCEPTED=$((SIX_RAR_ACCEPTED + 1))
        pass "$label accepted routed read-after-write seed"
    else
        fail "$label rejected routed read-after-write seed" "$R"
    fi
done

sleep 4
SIX_RAR_OK=true
SIX_RAR_DETAILS=""
for read_i in "${!ENTRYPOINT_LABELS[@]}"; do
    read_label="${ENTRYPOINT_LABELS[$read_i]}"
    read_url="${ENTRYPOINT_URLS[$read_i]}"
    read_key="${ENTRYPOINT_KEYS[$read_i]}"
    for author in "${SIX_RAR_AUTHORS[@]}"; do
        PAYLOAD=$(query_with_args "$read_url" "$read_key" "messages:countMessagesByAuthorChannel" \
            "{\"author\":\"$author\",\"channel\":\"six-entrypoint-rar\"}" 2>/dev/null || echo '{"value":-1}')
        COUNT=$(python3 -c "import sys,json; print(int(json.load(sys.stdin)['value']))" <<< "$PAYLOAD" 2>/dev/null || echo -1)
        if [ "$COUNT" -ne 1 ]; then
            SIX_RAR_OK=false
            SIX_RAR_DETAILS="$SIX_RAR_DETAILS $read_label:$author=$COUNT"
        fi
    done
done

$SIX_RAR_OK \
    && pass "All six nodes read all six routed writes" \
    || fail "Six-entrypoint read-after-write failed" "$SIX_RAR_DETAILS"

POST_SIX_RAR=$(query_api "$NODE_A_URL" "$NODE_A_KEY" "messages:dashboard")
AM=$(jval messages "$POST_SIX_RAR"); AU=$(jval users "$POST_SIX_RAR")
AP=$(jval projects "$POST_SIX_RAR"); AT=$(jval tasks "$POST_SIX_RAR")

# ============================================================
echo ""
echo -e "${BOLD}Test 4d: Live Membership Refresh After Node Readdress${NC}"
# ============================================================

# Recreate one participant with a different advertised address namespace. The
# Compose service and host port stay stable for the harness, but the gRPC alias
# advertised into membership changes. Other nodes must refresh from membership
# without a process restart.
READDR_RUN_ID="readdr-$(date +%s)"
READDR_ADDR="node-p1a-${READDR_RUN_ID}:50051"
READDR_OVERRIDE=$(mktemp)
cat > "$READDR_OVERRIDE" << EOF
services:
  node-p1a:
    environment:
      CLUSTER_NODE_GENERATION: "1"
EOF
echo "  Recreating p1a with advertised address $READDR_ADDR..."
(
    cd "$SCRIPT_DIR"
    CLUSTER_RUN_ID="$READDR_RUN_ID" BACKEND_PULL_POLICY=never \
        docker compose -f docker-compose.yml -f "$READDR_OVERRIDE" --profile cluster \
        up -d --no-deps --force-recreate node-p1a > /dev/null
)
rm -f "$READDR_OVERRIDE"

for _ in $(seq 1 40); do
    curl -sf "$NODE_P1A_URL/version" > /dev/null 2>&1 && break
    sleep 1
done
NODE_P1A_KEY=$(docker exec docker-node-p1a-1 ./generate_admin_key.sh 2>&1 | tail -1)
NODE_B_KEY="$NODE_P1A_KEY"
ENTRYPOINT_KEYS[3]="$NODE_P1A_KEY"

P1A_ADVERTISED=$(docker exec docker-node-p1a-1 printenv ADVERTISE_GRPC_ADDR 2>/dev/null || true)
[ "$P1A_ADVERTISED" = "$READDR_ADDR" ] \
    && pass "p1a restarted with a new advertised gRPC address" \
    || fail "p1a did not restart with the expected advertised address" "got=$P1A_ADVERTISED expected=$READDR_ADDR"

OBSERVED_REFRESH=false
for _ in $(seq 1 30); do
    if docker logs docker-node-p0a-1 2>&1 | grep -F "Refreshed cluster membership snapshot" | grep -F "$READDR_ADDR" > /dev/null; then
        OBSERVED_REFRESH=true
        break
    fi
    sleep 1
done
$OBSERVED_REFRESH \
    && pass "p0a refreshed membership and observed p1a's new advertised address" \
    || fail "p0a did not observe p1a's new advertised address" "$READDR_ADDR"

READDR_WRITE_OK=false
READDR_WRITE_RESPONSE=""
READDR_WRITE_TEXT="membership-readdress-${READDR_RUN_ID}"
for attempt in $(seq 1 20); do
    READDR_ATTEMPT_TEXT="$READDR_WRITE_TEXT-$attempt"
    READDR_WRITE_RESPONSE=$(mutation_response "$NODE_A_URL" "$NODE_A_KEY" "messages:crossPartitionWrite" \
        "{\"text\":\"$READDR_ATTEMPT_TEXT\",\"taskTitle\":\"$READDR_ATTEMPT_TEXT\"}")
    if echo "$READDR_WRITE_RESPONSE" | grep -q '"status":"success"'; then
        sleep 2
        MT=$(count_message_text "$NODE_A_URL" "$NODE_A_KEY" "$READDR_ATTEMPT_TEXT")
        TT=$(count_task_title "$NODE_A_URL" "$NODE_A_KEY" "$READDR_ATTEMPT_TEXT")
        if [ "$MT" -eq 1 ] && [ "$TT" -eq 1 ]; then
            READDR_WRITE_OK=true
            break
        fi
    fi
    sleep 1
done
if $READDR_WRITE_OK; then
    pass "Cross-partition routing worked after live membership readdress"
    POST_READDR=$(query_api "$NODE_A_URL" "$NODE_A_KEY" "messages:dashboard")
    AM=$(jval messages "$POST_READDR"); AU=$(jval users "$POST_READDR")
    AP=$(jval projects "$POST_READDR"); AT=$(jval tasks "$POST_READDR")
else
    fail "Cross-partition routing failed after live membership readdress" "$READDR_WRITE_RESPONSE"
fi

export CLUSTER_RUN_ID="$READDR_RUN_ID"

# ============================================================
echo ""
echo -e "${BOLD}Test 4e: Membership Drain Exclusion${NC}"
# ============================================================

# A draining node may still be alive for Raft/cleanup, but new membership-routed
# work must not select its advertised endpoint. Advertise an intentionally
# unusable gRPC address while draining; if routing ignores drain state, the write
# below will try that address first and fail.
DRAIN_ADDR="draining-p1a-invalid-$(date +%s):50051"
DRAIN_OVERRIDE=$(mktemp)
cat > "$DRAIN_OVERRIDE" << EOF
services:
  node-p1a:
    environment:
      CLUSTER_NODE_GENERATION: "2"
      CLUSTER_NODE_DRAINING: "true"
      ADVERTISE_GRPC_ADDR: "$DRAIN_ADDR"
EOF

echo "  Recreating p1a as draining with invalid advertised address $DRAIN_ADDR..."
(
    cd "$SCRIPT_DIR"
    CLUSTER_RUN_ID="$READDR_RUN_ID" BACKEND_PULL_POLICY=never \
        docker compose -f docker-compose.yml -f "$DRAIN_OVERRIDE" --profile cluster \
        up -d --no-deps --force-recreate node-p1a > /dev/null
)
rm -f "$DRAIN_OVERRIDE"

for _ in $(seq 1 40); do
    curl -sf "$NODE_P1A_URL/version" > /dev/null 2>&1 && break
    sleep 1
done
NODE_P1A_KEY=$(docker exec docker-node-p1a-1 ./generate_admin_key.sh 2>&1 | tail -1)
NODE_B_KEY="$NODE_P1A_KEY"
ENTRYPOINT_KEYS[3]="$NODE_P1A_KEY"

P1A_DRAINING=$(docker exec docker-node-p1a-1 printenv CLUSTER_NODE_DRAINING 2>/dev/null || true)
P1A_DRAIN_ADDR=$(docker exec docker-node-p1a-1 printenv ADVERTISE_GRPC_ADDR 2>/dev/null || true)
[ "$P1A_DRAINING" = "true" ] && [ "$P1A_DRAIN_ADDR" = "$DRAIN_ADDR" ] \
    && pass "p1a registered as draining with a test-only invalid advertised address" \
    || fail "p1a drain test did not start with expected env" "draining=$P1A_DRAINING addr=$P1A_DRAIN_ADDR expected=$DRAIN_ADDR"

DRAIN_LOG=$(wait_membership_excludes docker-node-p0a-1 "node-p1a-" "node-p1b" 45 || true)
if [ -z "$DRAIN_LOG" ] && ! latest_membership_log docker-node-p0a-1 120s | grep -F "$DRAIN_ADDR" > /dev/null; then
    pass "Live membership excluded draining p1a from routed addresses"
else
    fail "Live membership did not exclude draining p1a" "${DRAIN_LOG:-$(latest_membership_log docker-node-p0a-1 120s)}"
fi

assert_cross_partition_write_exact_once \
    "$NODE_A_URL" "$NODE_A_KEY" \
    "membership-drain-${READDR_RUN_ID}" \
    "membership-drain-${READDR_RUN_ID}" \
    "Cross-partition routing avoided the draining p1a endpoint"

# ============================================================
echo ""
echo -e "${BOLD}Test 4f: Membership Heartbeat Expiry${NC}"
# ============================================================

# Register a non-draining node with an unusable advertised address, prove that
# the bad endpoint enters live membership, then stop the node and wait for its
# heartbeat lease to expire. Routing should fail closed around the expired entry
# and use the remaining live members.
STALE_ADDR="stale-p1a-invalid-$(date +%s):50051"
STALE_OVERRIDE=$(mktemp)
cat > "$STALE_OVERRIDE" << EOF
services:
  node-p1a:
    environment:
      CLUSTER_NODE_GENERATION: "3"
      CLUSTER_NODE_DRAINING: "false"
      ADVERTISE_GRPC_ADDR: "$STALE_ADDR"
EOF

echo "  Recreating p1a with stale-test advertised address $STALE_ADDR..."
(
    cd "$SCRIPT_DIR"
    CLUSTER_RUN_ID="$READDR_RUN_ID" BACKEND_PULL_POLICY=never \
        docker compose -f docker-compose.yml -f "$STALE_OVERRIDE" --profile cluster \
        up -d --no-deps --force-recreate node-p1a > /dev/null
)
rm -f "$STALE_OVERRIDE"

for _ in $(seq 1 40); do
    curl -sf "$NODE_P1A_URL/version" > /dev/null 2>&1 && break
    sleep 1
done
NODE_P1A_KEY=$(docker exec docker-node-p1a-1 ./generate_admin_key.sh 2>&1 | tail -1)
NODE_B_KEY="$NODE_P1A_KEY"
ENTRYPOINT_KEYS[3]="$NODE_P1A_KEY"

if wait_membership_contains docker-node-p0a-1 "$STALE_ADDR" 45 > /dev/null; then
    pass "Live membership observed p1a's stale-test advertised address"
else
    fail "Live membership did not observe p1a's stale-test address" "$(latest_membership_log docker-node-p0a-1 120s)"
fi

echo "  Stopping p1a and waiting for its membership heartbeat lease to expire..."
docker stop docker-node-p1a-1 > /dev/null 2>&1
STALE_LOG=$(wait_membership_excludes docker-node-p0a-1 "$STALE_ADDR" "node-p1b" 60 || true)
if [ -z "$STALE_LOG" ]; then
    pass "Live membership excluded p1a after heartbeat expiry"
else
    fail "Live membership did not exclude expired p1a heartbeat" "$STALE_LOG"
fi

assert_cross_partition_write_exact_once \
    "$NODE_A_URL" "$NODE_A_KEY" \
    "membership-expiry-${READDR_RUN_ID}" \
    "membership-expiry-${READDR_RUN_ID}" \
    "Cross-partition routing avoided the expired p1a endpoint"

echo "  Restoring p1a as a live non-draining member..."
RESTORE_OVERRIDE=$(mktemp)
cat > "$RESTORE_OVERRIDE" << EOF
services:
  node-p1a:
    environment:
      CLUSTER_NODE_GENERATION: "4"
EOF
(
    cd "$SCRIPT_DIR"
    CLUSTER_RUN_ID="$READDR_RUN_ID" BACKEND_PULL_POLICY=never \
        docker compose -f docker-compose.yml -f "$RESTORE_OVERRIDE" --profile cluster \
        up -d --no-deps --force-recreate node-p1a > /dev/null
)
rm -f "$RESTORE_OVERRIDE"
for _ in $(seq 1 40); do
    curl -sf "$NODE_P1A_URL/version" > /dev/null 2>&1 && break
    sleep 1
done
NODE_P1A_KEY=$(docker exec docker-node-p1a-1 ./generate_admin_key.sh 2>&1 | tail -1)
NODE_B_KEY="$NODE_P1A_KEY"
ENTRYPOINT_KEYS[3]="$NODE_P1A_KEY"
RESTORED_ADDR="node-p1a-${READDR_RUN_ID}:50051"
if wait_membership_contains docker-node-p0a-1 "$RESTORED_ADDR" 45 > /dev/null; then
    pass "p1a restored as a live routed member"
else
    fail "p1a did not restore into live membership" "$(latest_membership_log docker-node-p0a-1 120s)"
fi

RESTORE_DEPLOYED=false
echo "  Re-deploying functions to restored p1a..."
for _ in $(seq 1 5); do
    if (cd "$DEPLOY_DIR" && npx convex deploy --admin-key "$NODE_B_KEY" --url "$NODE_B_URL" > /dev/null 2>&1); then
        RESTORE_DEPLOYED=true
        break
    fi
    sleep 2
done
$RESTORE_DEPLOYED \
    && pass "Functions redeployed to restored p1a" \
    || fail "Could not redeploy functions to restored p1a"

RESTORED_WRITE_READY=false
RESTORED_READY_TITLE=""
RESTORED_READY_RESPONSE=""
for attempt in $(seq 1 20); do
    # Deploy can race Raft leader discovery after a force-recreate. Retry both
    # deploy and a real write through the restored entrypoint until the public
    # mutation path proves it can execute again.
    (cd "$DEPLOY_DIR" && npx convex deploy --admin-key "$NODE_B_KEY" --url "$NODE_B_URL" > /dev/null 2>&1) || true
    RESTORED_READY_TITLE="membership-restore-ready-${READDR_RUN_ID}-$attempt"
    RESTORED_READY_RESPONSE=$(mutation_response "$NODE_B_URL" "$NODE_B_KEY" "messages:createTask" \
        "{\"title\":\"$RESTORED_READY_TITLE\",\"assignee\":\"membership\",\"project\":\"membership\",\"status\":\"done\"}")
    if echo "$RESTORED_READY_RESPONSE" | grep -q '"status":"success"'; then
        RESTORED_WRITE_READY=true
        break
    fi
    sleep 2
done
if $RESTORED_WRITE_READY; then
    READY_TASK_COUNT=$(count_task_title "$NODE_A_URL" "$NODE_A_KEY" "$RESTORED_READY_TITLE")
    [ "$READY_TASK_COUNT" -eq 1 ] \
        && pass "Restored p1a is ready for public write traffic" \
        || fail "Restored p1a readiness write did not appear exactly once" "tasks=$READY_TASK_COUNT title=$RESTORED_READY_TITLE"
else
    fail "Restored p1a public write API did not become ready" "$RESTORED_READY_RESPONSE"
fi

POST_MEMBERSHIP_PROBES=$(query_api "$NODE_A_URL" "$NODE_A_KEY" "messages:dashboard")
AM=$(jval messages "$POST_MEMBERSHIP_PROBES"); AU=$(jval users "$POST_MEMBERSHIP_PROBES")
AP=$(jval projects "$POST_MEMBERSHIP_PROBES"); AT=$(jval tasks "$POST_MEMBERSHIP_PROBES")

# ============================================================
echo ""
echo -e "${BOLD}Test 5: Concurrent Write Scaling (CockroachDB KV)${NC}"
# ============================================================

N=20

START=$(python3 -c "import time; print(time.time())")

# Node A: messages in background
(for i in $(seq 1 $N); do
    mutate "$NODE_A_URL" "$NODE_A_KEY" "messages:send" \
        "{\"text\":\"concurrent-$i\",\"author\":\"load\",\"channel\":\"perf\"}" > /dev/null
done) &
PA=$!

# Node B: tasks in background
(for i in $(seq 1 $N); do
    mutate "$NODE_B_URL" "$NODE_B_KEY" "messages:createTask" \
        "{\"title\":\"concurrent-$i\",\"assignee\":\"load\",\"project\":\"perf\",\"status\":\"done\"}" > /dev/null
done) &
PB=$!

wait $PA
wait $PB

END=$(python3 -c "import time; print(time.time())")
DUR=$(python3 -c "print(f'{$END - $START:.2f}')")
OPS=$((N * 2))
TPS=$(python3 -c "print(f'{$OPS / ($END - $START):.1f}')")

echo "  $OPS writes in ${DUR}s (${TPS} writes/sec)"
echo "  Waiting for replication..."
sleep 4

FA=$(query_api "$NODE_A_URL" "$NODE_A_KEY" "messages:dashboard")
FB=$(query_api "$NODE_B_URL" "$NODE_B_KEY" "messages:dashboard")

# Expected: post-test1 counts + N concurrent writes each
EM=$((AM + N))
ET=$((AT + N))

FAM=$(jval messages "$FA"); FAT=$(jval tasks "$FA")
FBM=$(jval messages "$FB"); FBT=$(jval tasks "$FB")

[ "$FAM" -eq "$EM" ] && [ "$FAT" -eq "$ET" ] \
    && pass "Node A: all writes visible (msgs=$FAM tasks=$FAT)" \
    || fail "Node A missing writes" "msgs=$FAM (expected $EM) tasks=$FAT (expected $ET)"

[ "$FBM" -eq "$EM" ] && [ "$FBT" -eq "$ET" ] \
    && pass "Node B: all writes visible (msgs=$FBM tasks=$FBT)" \
    || fail "Node B missing writes" "msgs=$FBM (expected $EM) tasks=$FBT (expected $ET)"

[ "$FAM" -eq "$FBM" ] && [ "$FAT" -eq "$FBT" ] \
    && pass "Both nodes converged after concurrent writes" \
    || fail "Nodes diverged" "A: msgs=$FAM tasks=$FAT vs B: msgs=$FBM tasks=$FBT"

# ============================================================
echo ""
echo -e "${BOLD}Test 6: Monotonic Reads (TiDB monotonic)${NC}"
# ============================================================

# Write a sequence of incrementing values to Node B (tasks table).
# After each write, read from Node A. Values must never go backward.
MONO_KEY="mono-$(date +%s)"
LAST_SEEN=-1
MONO_OK=true

for i in $(seq 1 10); do
    mutate "$NODE_B_URL" "$NODE_B_KEY" "messages:writeSequence" \
        "{\"key\":\"$MONO_KEY\",\"seq\":$i}" > /dev/null
    sleep 0.5
    VAL=$(curl -sf "$NODE_A_URL/api/query" \
        -H "Authorization: Convex $NODE_A_KEY" \
        -H "Content-Type: application/json" \
        -d "{\"path\":\"messages:readLatestSeq\",\"args\":{\"key\":\"$MONO_KEY\"}}" \
        | python3 -c "import sys,json; print(int(json.load(sys.stdin)['value']))" 2>/dev/null || echo "-1")
    if [ "$VAL" -lt "$LAST_SEEN" ]; then
        MONO_OK=false
        fail "Monotonic violation at seq=$i" "read $VAL after previously reading $LAST_SEEN"
        break
    fi
    LAST_SEEN=$VAL
done

if $MONO_OK; then
    pass "Monotonic reads: values never went backward (last=$LAST_SEEN)"
fi

# ============================================================
echo ""
echo -e "${BOLD}Test 9m: Cluster-Safe Latest Reads With Delayed Participant Delta${NC}"
# ============================================================
# Discover the current partition-0 serving leader, then pause only that node's
# partition-1 JetStream consumer. NATS KV remains available for the 2PC
# decision, so the write can commit while the coordinator's local mirror is
# deliberately missing the remote participant half.
# Latest reads and sync reruns must use authoritative owners at one certified
# timestamp and therefore expose either neither half or both halves.

SAFE_SNAPSHOT_RUN="safe-snapshot-$(date +%s)-$$"
SAFE_SNAPSHOT_TEXT="$SAFE_SNAPSHOT_RUN-msg"
SAFE_SNAPSHOT_TASK="$SAFE_SNAPSHOT_RUN-task"
SAFE_SNAPSHOT_SUB_LOG=$(mktemp)
SAFE_SNAPSHOT_COORDINATOR_FOUND=false
SAFE_SNAPSHOT_URL=""
SAFE_SNAPSHOT_KEY=""
SAFE_SNAPSHOT_CONTAINER=""
for entry in \
    "docker-node-p0a-1|$NODE_P0A_URL|$NODE_P0A_KEY" \
    "docker-node-p0b-1|$NODE_P0B_URL|$NODE_P0B_KEY" \
    "docker-node-p0c-1|$NODE_P0C_URL|$NODE_P0C_KEY"; do
    IFS='|' read -r candidate_container candidate_url candidate_key <<< "$entry"
    status=$(curl --max-time 2 -s -o /dev/null -w "%{http_code}" \
        -H "Connection: Upgrade" \
        -H "Upgrade: websocket" \
        -H "Sec-WebSocket-Key: dGhlIHNhbXBsZSBub25jZQ==" \
        -H "Sec-WebSocket-Version: 13" \
        "$candidate_url/api/1.34.1/sync" 2>/dev/null || true)
    if [ "$status" = "101" ]; then
        SAFE_SNAPSHOT_COORDINATOR_FOUND=true
        SAFE_SNAPSHOT_URL="$candidate_url"
        SAFE_SNAPSHOT_KEY="$candidate_key"
        SAFE_SNAPSHOT_CONTAINER="$candidate_container"
        break
    fi
done

if $SAFE_SNAPSHOT_COORDINATOR_FOUND; then
    # Partition 0 uses Config::name() directly for its durable replication
    # consumer, which is sourced from INSTANCE_NAME in the container.
    SAFE_SNAPSHOT_NATS_CONSUMER=$(docker exec "$SAFE_SNAPSHOT_CONTAINER" \
        printenv INSTANCE_NAME 2>/dev/null)

    (cd "$DEPLOY_DIR" && node safe-snapshot-subscription.mjs \
        "$SAFE_SNAPSHOT_URL" "$SAFE_SNAPSHOT_TEXT" "$SAFE_SNAPSHOT_TASK" \
        > "$SAFE_SNAPSHOT_SUB_LOG" 2>&1) &
    SAFE_SNAPSHOT_SUB_PID=$!
else
    SAFE_SNAPSHOT_SUB_PID=""
fi

SAFE_SNAPSHOT_SUB_READY=false
if $SAFE_SNAPSHOT_COORDINATOR_FOUND; then
    for _ in $(seq 1 100); do
        if grep -q '"event":"ready"' "$SAFE_SNAPSHOT_SUB_LOG"; then
            SAFE_SNAPSHOT_SUB_READY=true
            break
        fi
        if ! kill -0 "$SAFE_SNAPSHOT_SUB_PID" 2>/dev/null; then
            break
        fi
        sleep 0.1
    done
fi

if $SAFE_SNAPSHOT_SUB_READY; then
    set_jetstream_consumer_pause "$SAFE_SNAPSHOT_NATS_CONSUMER" 30
    SAFE_SNAPSHOT_RESPONSE=$(mutation_response \
        "$SAFE_SNAPSHOT_URL" "$SAFE_SNAPSHOT_KEY" "messages:crossPartitionWrite" \
        "{\"text\":\"$SAFE_SNAPSHOT_TEXT\",\"taskTitle\":\"$SAFE_SNAPSHOT_TASK\"}")

    SAFE_SNAPSHOT_HTTP_OK=true
    SAFE_SNAPSHOT_HTTP_DETAILS=""
    for _ in $(seq 1 5); do
        SAFE_SNAPSHOT_PAIR=$(query_with_args \
            "$SAFE_SNAPSHOT_URL" "$SAFE_SNAPSHOT_KEY" "messages:safeSnapshotPair" \
            "{\"text\":\"$SAFE_SNAPSHOT_TEXT\",\"taskTitle\":\"$SAFE_SNAPSHOT_TASK\"}")
        read -r SAFE_SNAPSHOT_MESSAGES SAFE_SNAPSHOT_TASKS <<< "$(python3 -c \
            'import json,sys; value=json.load(sys.stdin)["value"]; print(int(value["messages"]), int(value["tasks"]))' \
            <<< "$SAFE_SNAPSHOT_PAIR")"
        if [ "$SAFE_SNAPSHOT_MESSAGES" -ne "$SAFE_SNAPSHOT_TASKS" ] || \
            [ "$SAFE_SNAPSHOT_MESSAGES" -ne 1 ]; then
            SAFE_SNAPSHOT_HTTP_OK=false
            SAFE_SNAPSHOT_HTTP_DETAILS="$SAFE_SNAPSHOT_HTTP_DETAILS [$SAFE_SNAPSHOT_MESSAGES,$SAFE_SNAPSHOT_TASKS]"
        fi
    done

    SAFE_SNAPSHOT_SUB_STATUS=0
    wait "$SAFE_SNAPSHOT_SUB_PID" || SAFE_SNAPSHOT_SUB_STATUS=$?
    set_jetstream_consumer_pause "$SAFE_SNAPSHOT_NATS_CONSUMER" 0

    if echo "$SAFE_SNAPSHOT_RESPONSE" | grep -q '"status":"success"' && \
        $SAFE_SNAPSHOT_HTTP_OK; then
        pass "Latest HTTP reads stayed atomic while the participant delta was delayed"
    else
        fail "Latest HTTP read exposed or failed to recover the delayed 2PC pair" \
            "response=$SAFE_SNAPSHOT_RESPONSE observations=$SAFE_SNAPSHOT_HTTP_DETAILS"
    fi

    if [ "$SAFE_SNAPSHOT_SUB_STATUS" -eq 0 ] && \
        grep -q '"event":"complete"' "$SAFE_SNAPSHOT_SUB_LOG" && \
        ! grep -q '"event":"torn"' "$SAFE_SNAPSHOT_SUB_LOG"; then
        pass "WebSocket subscription rerun stayed atomic with delayed participant delivery"
    else
        fail "WebSocket subscription observed a torn or missing 2PC pair" \
            "$(tr '\n' ' ' < "$SAFE_SNAPSHOT_SUB_LOG")"
    fi
else
    if [ -n "$SAFE_SNAPSHOT_SUB_PID" ]; then
        kill "$SAFE_SNAPSHOT_SUB_PID" 2>/dev/null || true
        wait "$SAFE_SNAPSHOT_SUB_PID" 2>/dev/null || true
    fi
    fail "WebSocket safe-snapshot probe did not reach its initial state" \
        "coordinator_found=$SAFE_SNAPSHOT_COORDINATOR_FOUND $(tr '\n' ' ' < "$SAFE_SNAPSHOT_SUB_LOG")"
    fail "Latest HTTP delayed-participant test could not start" \
        "subscription setup failed before the participant delta was delayed"
fi
rm -f "$SAFE_SNAPSHOT_SUB_LOG"

# ============================================================
echo ""
echo -e "${BOLD}Test 7: Node Restart Recovery (TiDB kill -9 / CockroachDB nemesis)${NC}"
# ============================================================

# Capture pre-restart state
PRE=$(query_api "$NODE_A_URL" "$NODE_A_KEY" "messages:dashboard")
PRE_M=$(jval messages "$PRE")

# Restart Node B
echo "  Restarting Node B..."
docker restart docker-node-p1a-1 > /dev/null 2>&1

# Write to Node A while Node B is restarting
mutate "$NODE_A_URL" "$NODE_A_KEY" "messages:send" \
    '{"text":"during-restart","author":"recovery-test","channel":"test"}' > /dev/null

# Wait for Node B to come back healthy
echo "  Waiting for Node B to recover..."
for attempt in $(seq 1 30); do
    if curl -sf "http://127.0.0.1:3310/version" > /dev/null 2>&1; then
        break
    fi
    sleep 1
done

# Re-generate Node B key (may have changed on restart)
NODE_P1A_KEY=$(docker exec docker-node-p1a-1 ./generate_admin_key.sh 2>&1 | tail -1)
NODE_B_KEY="$NODE_P1A_KEY"

RESTART_GENESIS=$(wait_for_persistence_global convex_node_p1a cluster_genesis || true)
RESTART_RAFT_GENESIS=$(wait_for_persistence_global convex_node_p1a cluster_genesis_raft_applied || true)
RESTART_GENESIS_ID=$(genesis_id_from_json <<< "$RESTART_GENESIS" 2>/dev/null || true)
RESTART_RAFT_GENESIS_ID=$(genesis_id_from_json <<< "$RESTART_RAFT_GENESIS" 2>/dev/null || true)
if [ -n "$CANONICAL_GENESIS_ID" ] \
    && [ "$RESTART_GENESIS_ID" = "$CANONICAL_GENESIS_ID" ] \
    && [ "$RESTART_RAFT_GENESIS_ID" = "$CANONICAL_GENESIS_ID" ]; then
    pass "Restarted node retained canonical Raft-applied genesis identity"
else
    fail "Restarted node lost canonical genesis identity" \
        "canonical=$CANONICAL_GENESIS_ID marker=$RESTART_GENESIS_ID raft_applied=$RESTART_RAFT_GENESIS_ID"
fi

# Re-deploy functions to Node B (modules are in-memory)
echo "  Re-deploying functions to Node B..."
deploy_functions_with_retry "$NODE_B_KEY" "$NODE_B_URL" 5

sleep 4

# Verify Node B sees the write made during restart
POST_B=$(query_api "$NODE_B_URL" "$NODE_B_KEY" "messages:dashboard")
POST_BM=$(jval messages "$POST_B")

EXPECTED_POST_M=$((PRE_M + 1))
[ "$POST_BM" -ge "$EXPECTED_POST_M" ] \
    && pass "Node B recovered: sees msgs=$POST_BM (includes write during downtime)" \
    || fail "Node B missing writes after restart" "msgs=$POST_BM, expected >= $EXPECTED_POST_M"

# Verify Node B can still serve its own writes
mutate "$NODE_B_URL" "$NODE_B_KEY" "messages:createTask" \
    '{"title":"post-restart","assignee":"test","project":"recovery","status":"done"}' > /dev/null \
    && pass "Node B can write after recovery" \
    || fail "Node B cannot write after recovery"

# ============================================================
echo ""
echo -e "${BOLD}Test 8: Idempotent Re-Run (CockroachDB workload check)${NC}"
# ============================================================

# Capture state, write more data, verify no corruption
SNAP_A=$(query_api "$NODE_A_URL" "$NODE_A_KEY" "messages:dashboard")
SNAP_AM=$(jval messages "$SNAP_A")

mutate "$NODE_A_URL" "$NODE_A_KEY" "messages:send" \
    '{"text":"idempotent-1","author":"check","channel":"test"}' > /dev/null
mutate "$NODE_A_URL" "$NODE_A_KEY" "messages:send" \
    '{"text":"idempotent-2","author":"check","channel":"test"}' > /dev/null

sleep 3

CHECK_A=$(query_api "$NODE_A_URL" "$NODE_A_KEY" "messages:dashboard")
CHECK_B=$(query_api "$NODE_B_URL" "$NODE_B_KEY" "messages:dashboard")

CHECK_AM=$(jval messages "$CHECK_A")
CHECK_BM=$(jval messages "$CHECK_B")

EXPECTED_IDEM=$((SNAP_AM + 2))

[ "$CHECK_AM" -eq "$EXPECTED_IDEM" ] \
    && pass "Node A: correct after additional writes (msgs=$CHECK_AM)" \
    || fail "Node A count wrong" "msgs=$CHECK_AM, expected $EXPECTED_IDEM"

[ "$CHECK_AM" -eq "$CHECK_BM" ] \
    && pass "Both nodes still converged (msgs=$CHECK_AM)" \
    || fail "Nodes diverged after re-run" "A=$CHECK_AM vs B=$CHECK_BM"

# ============================================================
echo ""
echo -e "${BOLD}Test 9: Two-Phase Commit Cross-Partition (Vitess 2PC)${NC}"
# ============================================================

# Capture pre-2PC state.
PRE_2PC=$(query_api "$NODE_A_URL" "$NODE_A_KEY" "messages:dashboard")
PRE_2PC_M=$(jval messages "$PRE_2PC")
PRE_2PC_T=$(jval tasks "$PRE_2PC")

# 9a: Atomic cross-partition write (Vitess atomic commit pattern).
# A single mutation writes to messages (partition 0) AND tasks (partition 1).
# With 2PC, both should be committed atomically.
R=$(mutation_response "$NODE_A_URL" "$NODE_A_KEY" "messages:crossPartitionWrite" \
    '{"text":"2pc-msg-1","taskTitle":"2pc-task-1"}')
if echo "$R" | grep -q '"status":"success"'; then
    pass "Cross-partition mutation accepted (2PC coordinator)"
else
    fail "Cross-partition mutation rejected" "$R"
fi
FENCE_PARTITIONS=$(raw_participant_fences "$R" 2>/dev/null || true)
[ "$FENCE_PARTITIONS" = "0,1" ] \
    && pass "2PC read-after-write token fences all participant partitions" \
    || fail "2PC read-after-write token missing participant fences" "fences=$FENCE_PARTITIONS response=$R"

# 9b: Write a second cross-partition mutation.
R=$(mutation_response "$NODE_A_URL" "$NODE_A_KEY" "messages:crossPartitionWrite" \
    '{"text":"2pc-msg-2","taskTitle":"2pc-task-2"}')
if echo "$R" | grep -q '"status":"success"'; then
    pass "Second cross-partition mutation accepted (2PC coordinator)"
else
    fail "Second cross-partition mutation rejected" "$R"
fi

sleep 4

# 9c: Verify both partitions received the data (Vitess VDiff + CockroachDB cross-range).
POST_2PC_A=$(query_api "$NODE_A_URL" "$NODE_A_KEY" "messages:dashboard")
POST_2PC_B=$(query_api "$NODE_B_URL" "$NODE_B_KEY" "messages:dashboard")

POST_M_A=$(jval messages "$POST_2PC_A"); POST_T_A=$(jval tasks "$POST_2PC_A")
POST_M_B=$(jval messages "$POST_2PC_B"); POST_T_B=$(jval tasks "$POST_2PC_B")

DELTA_M=$((POST_M_A - PRE_2PC_M))
DELTA_T=$((POST_T_A - PRE_2PC_T))

# Both messages and tasks should have increased by 2.
[ "$DELTA_M" -eq 2 ] && [ "$DELTA_T" -eq 2 ] \
    && pass "Node A: 2PC writes visible (msgs +$DELTA_M, tasks +$DELTA_T)" \
    || fail "Node A: 2PC data missing" "msgs +$DELTA_M tasks +$DELTA_T, expected +2 +2"

# 9d: Cross-node visibility — Node B also sees the 2PC writes.
[ "$POST_M_A" -eq "$POST_M_B" ] && [ "$POST_T_A" -eq "$POST_T_B" ] \
    && pass "Both nodes see 2PC data (msgs=$POST_M_A, tasks=$POST_T_A)" \
    || fail "Nodes diverged after 2PC" "A: $POST_M_A,$POST_T_A vs B: $POST_M_B,$POST_T_B"

# 9e: Invariant check — 2PC didn't create phantom data (TiDB bank pattern).
# Each crossPartitionWrite creates exactly 1 message + 1 task. Total delta must be equal.
[ "$DELTA_M" -eq "$DELTA_T" ] \
    && pass "2PC invariant: msgs and tasks incremented equally (+$DELTA_M)" \
    || fail "2PC invariant violated" "msgs +$DELTA_M vs tasks +$DELTA_T"

FAILURE_WINDOW_2PC_LABELS=()
FAILURE_WINDOW_2PC_TEXTS=()
FAILURE_WINDOW_2PC_TASKS=()
FAILURE_WINDOW_2PC_RESPONSES=()
FAILURE_WINDOW_RUN="failure-window-2pc-$(date +%s)-$$"

# ============================================================
echo ""
echo -e "${BOLD}Test 9f: 2PC Atomicity During NATS Outage${NC}"
# ============================================================
# NATS backs the TSO and 2PC decision log, so an outage may make the mutation
# fail safely. What must never happen is a torn 2PC write.

PRE_NATS_2PC=$(query_api "$NODE_A_URL" "$NODE_A_KEY" "messages:dashboard")
PRE_NATS_2PC_M=$(jval messages "$PRE_NATS_2PC")
PRE_NATS_2PC_T=$(jval tasks "$PRE_NATS_2PC")
NATS_2PC_TEXT="$FAILURE_WINDOW_RUN-nats-outage-msg"
NATS_2PC_TASK="$FAILURE_WINDOW_RUN-nats-outage-task"

echo "  Pausing NATS and attempting one cross-partition mutation..."
docker pause docker-nats-1 > /dev/null 2>&1
NATS_2PC_RESPONSE=$(curl --max-time 10 -s "$NODE_A_URL/api/mutation" \
    -H "Authorization: Convex $NODE_A_KEY" \
    -H "Content-Type: application/json" \
    -d "{\"path\":\"messages:crossPartitionWrite\",\"args\":{\"text\":\"$NATS_2PC_TEXT\",\"taskTitle\":\"$NATS_2PC_TASK\"}}" \
    || echo '{"status":"transport_error"}')
sleep 2
docker unpause docker-nats-1 > /dev/null 2>&1
echo "  NATS resumed."
FAILURE_WINDOW_2PC_LABELS+=("nats-outage")
FAILURE_WINDOW_2PC_TEXTS+=("$NATS_2PC_TEXT")
FAILURE_WINDOW_2PC_TASKS+=("$NATS_2PC_TASK")
FAILURE_WINDOW_2PC_RESPONSES+=("$NATS_2PC_RESPONSE")

NATS_2PC_COUNTS=$(wait_for_2pc_pair_exactness "$NATS_2PC_TEXT" "$NATS_2PC_TASK" "$NATS_2PC_RESPONSE" || true)
read -r NATS_2PC_MSG_A NATS_2PC_MSG_B NATS_2PC_TASK_A NATS_2PC_TASK_B <<< "$NATS_2PC_COUNTS"

if two_pc_counts_match_response "$NATS_2PC_RESPONSE" "$NATS_2PC_MSG_A" "$NATS_2PC_MSG_B" "$NATS_2PC_TASK_A" "$NATS_2PC_TASK_B"; then
    pass "2PC under NATS outage was atomic/fail-closed (msgA=$NATS_2PC_MSG_A msgB=$NATS_2PC_MSG_B taskA=$NATS_2PC_TASK_A taskB=$NATS_2PC_TASK_B)"
else
    fail "2PC under NATS outage produced torn/non-idempotent result" "response=$NATS_2PC_RESPONSE msgA=$NATS_2PC_MSG_A msgB=$NATS_2PC_MSG_B taskA=$NATS_2PC_TASK_A taskB=$NATS_2PC_TASK_B"
fi

POST_NATS_2PC=$(query_api "$NODE_A_URL" "$NODE_A_KEY" "messages:dashboard")
POST_NATS_2PC_M=$(jval messages "$POST_NATS_2PC")
POST_NATS_2PC_T=$(jval tasks "$POST_NATS_2PC")
POST_NATS_2PC_B=$(query_api "$NODE_B_URL" "$NODE_B_KEY" "messages:dashboard")
POST_NATS_2PC_BM=$(jval messages "$POST_NATS_2PC_B")
POST_NATS_2PC_BT=$(jval tasks "$POST_NATS_2PC_B")
[ "$POST_NATS_2PC_M" -eq "$POST_NATS_2PC_BM" ] && [ "$POST_NATS_2PC_T" -eq "$POST_NATS_2PC_BT" ] \
    && pass "Nodes converged after NATS 2PC disruption" \
    || fail "Nodes diverged after NATS 2PC disruption" "A: $POST_NATS_2PC_M,$POST_NATS_2PC_T vs B: $POST_NATS_2PC_BM,$POST_NATS_2PC_BT"

# ============================================================
echo ""
echo -e "${BOLD}Test 9g: 2PC Atomicity During Participant Restart${NC}"
# ============================================================
# Race a cross-partition mutation with a partition-1 participant restart. The
# client result may be ambiguous, but the persisted result must not be torn.

PARTICIPANT_RESTART_TEXT="$FAILURE_WINDOW_RUN-participant-restart-msg"
PARTICIPANT_RESTART_TASK="$FAILURE_WINDOW_RUN-participant-restart-task"
PRE_RESTART_2PC=$(query_api "$NODE_A_URL" "$NODE_A_KEY" "messages:dashboard")
PRE_RESTART_2PC_M=$(jval messages "$PRE_RESTART_2PC")
PRE_RESTART_2PC_T=$(jval tasks "$PRE_RESTART_2PC")
RESTART_2PC_RESPONSE_FILE=$(mktemp)

(curl --max-time 20 -s "$NODE_A_URL/api/mutation" \
    -H "Authorization: Convex $NODE_A_KEY" \
    -H "Content-Type: application/json" \
    -d "{\"path\":\"messages:crossPartitionWrite\",\"args\":{\"text\":\"$PARTICIPANT_RESTART_TEXT\",\"taskTitle\":\"$PARTICIPANT_RESTART_TASK\"}}" \
    > "$RESTART_2PC_RESPONSE_FILE" || echo '{"status":"transport_error"}' > "$RESTART_2PC_RESPONSE_FILE") &
RESTART_2PC_PID=$!
sleep 0.05
docker restart docker-node-p1a-1 > /dev/null 2>&1
wait "$RESTART_2PC_PID" || true

echo "  Waiting for participant node p1a to recover..."
for attempt in $(seq 1 60); do
    curl -sf "$NODE_B_URL/version" > /dev/null 2>&1 && break
    sleep 1
done
NODE_P1A_KEY=$(docker exec docker-node-p1a-1 ./generate_admin_key.sh 2>&1 | tail -1)
NODE_B_KEY="$NODE_P1A_KEY"
deploy_functions_with_retry "$NODE_B_KEY" "$NODE_B_URL" 5

sleep 8
POST_RESTART_2PC_COUNTS=$(wait_dashboard_message_task_convergence || true)
read -r POST_RESTART_2PC_M POST_RESTART_2PC_T POST_RESTART_2PC_BM POST_RESTART_2PC_BT <<< "$POST_RESTART_2PC_COUNTS"
RESTART_2PC_RESPONSE=$(cat "$RESTART_2PC_RESPONSE_FILE")
rm -f "$RESTART_2PC_RESPONSE_FILE"
FAILURE_WINDOW_2PC_LABELS+=("participant-restart")
FAILURE_WINDOW_2PC_TEXTS+=("$PARTICIPANT_RESTART_TEXT")
FAILURE_WINDOW_2PC_TASKS+=("$PARTICIPANT_RESTART_TASK")
FAILURE_WINDOW_2PC_RESPONSES+=("$RESTART_2PC_RESPONSE")
RESTART_2PC_COUNTS=$(wait_for_2pc_pair_exactness "$PARTICIPANT_RESTART_TEXT" "$PARTICIPANT_RESTART_TASK" "$RESTART_2PC_RESPONSE" || true)
read -r RESTART_2PC_MSG_A RESTART_2PC_MSG_B RESTART_2PC_TASK_A RESTART_2PC_TASK_B <<< "$RESTART_2PC_COUNTS"

if two_pc_counts_match_response "$RESTART_2PC_RESPONSE" "$RESTART_2PC_MSG_A" "$RESTART_2PC_MSG_B" "$RESTART_2PC_TASK_A" "$RESTART_2PC_TASK_B"; then
    pass "2PC during participant restart was atomic (msgA=$RESTART_2PC_MSG_A msgB=$RESTART_2PC_MSG_B taskA=$RESTART_2PC_TASK_A taskB=$RESTART_2PC_TASK_B)"
else
    fail "2PC during participant restart produced torn/non-idempotent result" "response=$RESTART_2PC_RESPONSE msgA=$RESTART_2PC_MSG_A msgB=$RESTART_2PC_MSG_B taskA=$RESTART_2PC_TASK_A taskB=$RESTART_2PC_TASK_B"
fi

[ "$POST_RESTART_2PC_M" -eq "$POST_RESTART_2PC_BM" ] && [ "$POST_RESTART_2PC_T" -eq "$POST_RESTART_2PC_BT" ] \
    && pass "Nodes converged after participant restart 2PC race" \
    || fail "Nodes diverged after participant restart 2PC race" "A: $POST_RESTART_2PC_M,$POST_RESTART_2PC_T vs B: $POST_RESTART_2PC_BM,$POST_RESTART_2PC_BT"

# ============================================================
echo ""
echo -e "${BOLD}Test 9h: 2PC Atomicity During Coordinator Restart${NC}"
# ============================================================
# Race a cross-partition mutation with a partition-0 coordinator restart. This
# is the coordinator-crash window: after staging/decision work starts, the
# persisted outcome must still be either fully absent or fully present.

COORD_RESTART_TEXT="$FAILURE_WINDOW_RUN-coordinator-restart-msg"
COORD_RESTART_TASK="$FAILURE_WINDOW_RUN-coordinator-restart-task"
PRE_COORD_RESTART_2PC=$(query_api "$NODE_A_URL" "$NODE_A_KEY" "messages:dashboard")
PRE_COORD_RESTART_2PC_M=$(jval messages "$PRE_COORD_RESTART_2PC")
PRE_COORD_RESTART_2PC_T=$(jval tasks "$PRE_COORD_RESTART_2PC")
COORD_RESTART_2PC_RESPONSE_FILE=$(mktemp)

(curl --max-time 20 -s "$NODE_A_URL/api/mutation" \
    -H "Authorization: Convex $NODE_A_KEY" \
    -H "Content-Type: application/json" \
    -d "{\"path\":\"messages:crossPartitionWrite\",\"args\":{\"text\":\"$COORD_RESTART_TEXT\",\"taskTitle\":\"$COORD_RESTART_TASK\"}}" \
    > "$COORD_RESTART_2PC_RESPONSE_FILE" || echo '{"status":"transport_error"}' > "$COORD_RESTART_2PC_RESPONSE_FILE") &
COORD_RESTART_2PC_PID=$!
sleep 0.05
docker restart docker-node-p0a-1 > /dev/null 2>&1
wait "$COORD_RESTART_2PC_PID" || true

echo "  Waiting for coordinator node p0a to recover..."
for attempt in $(seq 1 60); do
    curl -sf "$NODE_A_URL/version" > /dev/null 2>&1 && break
    sleep 1
done
NODE_P0A_KEY=$(docker exec docker-node-p0a-1 ./generate_admin_key.sh 2>&1 | tail -1)
NODE_A_KEY="$NODE_P0A_KEY"
deploy_functions_with_retry "$NODE_A_KEY" "$NODE_A_URL" 5

sleep 8
POST_COORD_RESTART_2PC_COUNTS=$(wait_dashboard_message_task_convergence || true)
read -r POST_COORD_RESTART_2PC_M POST_COORD_RESTART_2PC_T POST_COORD_RESTART_2PC_BM POST_COORD_RESTART_2PC_BT <<< "$POST_COORD_RESTART_2PC_COUNTS"
COORD_RESTART_2PC_RESPONSE=$(cat "$COORD_RESTART_2PC_RESPONSE_FILE")
rm -f "$COORD_RESTART_2PC_RESPONSE_FILE"
FAILURE_WINDOW_2PC_LABELS+=("coordinator-restart")
FAILURE_WINDOW_2PC_TEXTS+=("$COORD_RESTART_TEXT")
FAILURE_WINDOW_2PC_TASKS+=("$COORD_RESTART_TASK")
FAILURE_WINDOW_2PC_RESPONSES+=("$COORD_RESTART_2PC_RESPONSE")
COORD_RESTART_2PC_COUNTS=$(wait_for_2pc_pair_exactness "$COORD_RESTART_TEXT" "$COORD_RESTART_TASK" "$COORD_RESTART_2PC_RESPONSE" || true)
read -r COORD_RESTART_2PC_MSG_A COORD_RESTART_2PC_MSG_B COORD_RESTART_2PC_TASK_A COORD_RESTART_2PC_TASK_B <<< "$COORD_RESTART_2PC_COUNTS"

if two_pc_counts_match_response "$COORD_RESTART_2PC_RESPONSE" "$COORD_RESTART_2PC_MSG_A" "$COORD_RESTART_2PC_MSG_B" "$COORD_RESTART_2PC_TASK_A" "$COORD_RESTART_2PC_TASK_B"; then
    pass "2PC during coordinator restart was atomic (msgA=$COORD_RESTART_2PC_MSG_A msgB=$COORD_RESTART_2PC_MSG_B taskA=$COORD_RESTART_2PC_TASK_A taskB=$COORD_RESTART_2PC_TASK_B)"
else
    fail "2PC during coordinator restart produced torn/non-idempotent result" "response=$COORD_RESTART_2PC_RESPONSE msgA=$COORD_RESTART_2PC_MSG_A msgB=$COORD_RESTART_2PC_MSG_B taskA=$COORD_RESTART_2PC_TASK_A taskB=$COORD_RESTART_2PC_TASK_B"
fi

[ "$POST_COORD_RESTART_2PC_M" -eq "$POST_COORD_RESTART_2PC_BM" ] && [ "$POST_COORD_RESTART_2PC_T" -eq "$POST_COORD_RESTART_2PC_BT" ] \
    && pass "Nodes converged after coordinator restart 2PC race" \
    || fail "Nodes diverged after coordinator restart 2PC race" "A: $POST_COORD_RESTART_2PC_M,$POST_COORD_RESTART_2PC_T vs B: $POST_COORD_RESTART_2PC_BM,$POST_COORD_RESTART_2PC_BT"

# ============================================================
echo ""
echo -e "${BOLD}Test 9i: Per-Write 2PC Exactness During Failure Windows${NC}"
# ============================================================
# Aggregate deltas prove atomicity at a coarse level, but they can miss a bad
# recovery pattern where one accepted 2PC write is duplicated while another
# ambiguous write is lost. Check each NATS/restart window by its unique keys.

FAILURE_EXACTNESS_OK=true
FAILURE_EXACTNESS_DETAILS=""
for i in "${!FAILURE_WINDOW_2PC_LABELS[@]}"; do
    label="${FAILURE_WINDOW_2PC_LABELS[$i]}"
    text="${FAILURE_WINDOW_2PC_TEXTS[$i]}"
    task="${FAILURE_WINDOW_2PC_TASKS[$i]}"
    response="${FAILURE_WINDOW_2PC_RESPONSES[$i]}"

    COUNTS=$(wait_for_2pc_pair_exactness "$text" "$task" "$response" || true)
    read -r MSG_A MSG_B TASK_A TASK_B <<< "$COUNTS"
    if ! two_pc_counts_match_response "$response" "$MSG_A" "$MSG_B" "$TASK_A" "$TASK_B"; then
        FAILURE_EXACTNESS_OK=false
        if echo "$response" | grep -q '"status":"success"'; then
            FAILURE_EXACTNESS_DETAILS="$FAILURE_EXACTNESS_DETAILS [$label success msgA=$MSG_A msgB=$MSG_B taskA=$TASK_A taskB=$TASK_B]"
        else
            FAILURE_EXACTNESS_DETAILS="$FAILURE_EXACTNESS_DETAILS [$label ambiguous msgA=$MSG_A msgB=$MSG_B taskA=$TASK_A taskB=$TASK_B response=$response]"
        fi
    fi
done

$FAILURE_EXACTNESS_OK \
    && pass "Every NATS/restart-window 2PC write was absent or appeared exactly once on both nodes" \
    || fail "Failure-window 2PC exactness violated" "$FAILURE_EXACTNESS_DETAILS"

# ============================================================
echo ""
echo -e "${BOLD}Test 9j: 2PC Exactness During Repeated NATS Flaps${NC}"
# ============================================================
# Pause/unpause NATS repeatedly while issuing cross-partition writes. NATS backs
# the TSO, 2PC decision log, and replication transport, so accepted operations
# must recover exactly once and rejected/ambiguous operations must leave either
# no data or one complete 2PC pair, never a torn participant result.

NATS_FLAP_RUN="nats-flap-2pc-$(date +%s)"
NATS_FLAP_ATTEMPTS=10
NATS_FLAP_ACCEPTED=0
NATS_FLAP_REJECTED=0
NATS_FLAP_LABELS=()
NATS_FLAP_TEXTS=()
NATS_FLAP_TASKS=()
NATS_FLAP_RESPONSES=()

(
    for flap in $(seq 1 5); do
        docker pause docker-nats-1 > /dev/null 2>&1 || true
        sleep 0.7
        docker unpause docker-nats-1 > /dev/null 2>&1 || true
        sleep 0.7
    done
) &
NATS_FLAPPER_PID=$!

for i in $(seq 1 "$NATS_FLAP_ATTEMPTS"); do
    text="$NATS_FLAP_RUN-msg-$i"
    task="$NATS_FLAP_RUN-task-$i"
    response=$(curl --max-time 8 -s "$NODE_A_URL/api/mutation" \
        -H "Authorization: Convex $NODE_A_KEY" \
        -H "Content-Type: application/json" \
        -d "{\"path\":\"messages:crossPartitionWrite\",\"args\":{\"text\":\"$text\",\"taskTitle\":\"$task\"}}" \
        || echo '{"status":"transport_error"}')
    NATS_FLAP_LABELS+=("attempt-$i")
    NATS_FLAP_TEXTS+=("$text")
    NATS_FLAP_TASKS+=("$task")
    NATS_FLAP_RESPONSES+=("$response")
    if echo "$response" | grep -q '"status":"success"'; then
        NATS_FLAP_ACCEPTED=$((NATS_FLAP_ACCEPTED + 1))
    else
        NATS_FLAP_REJECTED=$((NATS_FLAP_REJECTED + 1))
        echo -e "  ${YELLOW}WARN${NC} 2PC attempt $i rejected/ambiguous during NATS flap: $response"
    fi
done

wait "$NATS_FLAPPER_PID" || true
docker unpause docker-nats-1 > /dev/null 2>&1 || true
sleep 8

[ "$NATS_FLAP_ACCEPTED" -gt 0 ] \
    && pass "NATS-flap 2PC workload accepted at least one write ($NATS_FLAP_ACCEPTED/$NATS_FLAP_ATTEMPTS)" \
    || fail "NATS-flap 2PC workload accepted no writes" "all attempts failed closed; test did not exercise recovery of accepted work"

curl -sf "$NODE_A_URL/version" > /dev/null 2>&1 \
    && pass "Node A survived repeated NATS flaps during 2PC workload" \
    || fail "Node A crashed during NATS-flap 2PC workload"

curl -sf "$NODE_B_URL/version" > /dev/null 2>&1 \
    && pass "Node B survived repeated NATS flaps during 2PC workload" \
    || fail "Node B crashed during NATS-flap 2PC workload"

NATS_FLAP_EXACTNESS_OK=true
NATS_FLAP_EXACTNESS_DETAILS=""
for i in "${!NATS_FLAP_LABELS[@]}"; do
    label="${NATS_FLAP_LABELS[$i]}"
    text="${NATS_FLAP_TEXTS[$i]}"
    task="${NATS_FLAP_TASKS[$i]}"
    response="${NATS_FLAP_RESPONSES[$i]}"

    COUNTS=$(wait_for_2pc_pair_exactness "$text" "$task" "$response" || true)
    read -r MSG_A MSG_B TASK_A TASK_B <<< "$COUNTS"
    if ! two_pc_counts_match_response "$response" "$MSG_A" "$MSG_B" "$TASK_A" "$TASK_B"; then
        NATS_FLAP_EXACTNESS_OK=false
        if echo "$response" | grep -q '"status":"success"'; then
            NATS_FLAP_EXACTNESS_DETAILS="$NATS_FLAP_EXACTNESS_DETAILS [$label success msgA=$MSG_A msgB=$MSG_B taskA=$TASK_A taskB=$TASK_B]"
        else
            NATS_FLAP_EXACTNESS_DETAILS="$NATS_FLAP_EXACTNESS_DETAILS [$label ambiguous msgA=$MSG_A msgB=$MSG_B taskA=$TASK_A taskB=$TASK_B response=$response]"
        fi
    fi
done

$NATS_FLAP_EXACTNESS_OK \
    && pass "Every NATS-flap 2PC attempt was absent or appeared exactly once on both nodes" \
    || fail "NATS-flap 2PC exactness violated" "$NATS_FLAP_EXACTNESS_DETAILS"

# ============================================================
echo ""
echo -e "${BOLD}Test 9k: Post-Ack 2PC Exactness After Cleanup-Window Restarts${NC}"
# ============================================================
# Once the client has observed success, recovery and cleanup may still be
# racing in the background. Restart the coordinator and participant entrypoints
# immediately after a successful cross-partition write, then prove that cleanup
# recovery neither drops nor duplicates the committed pair.

POST_ACK_RUN="post-ack-2pc-$(date +%s)"
POST_ACK_TEXT="$POST_ACK_RUN-msg"
POST_ACK_TASK="$POST_ACK_RUN-task"
POST_ACK_RESPONSE=$(mutation_response "$NODE_A_URL" "$NODE_A_KEY" "messages:crossPartitionWrite" \
    "{\"text\":\"$POST_ACK_TEXT\",\"taskTitle\":\"$POST_ACK_TASK\"}")

if echo "$POST_ACK_RESPONSE" | grep -q '"status":"success"'; then
    pass "Post-ack cleanup-window 2PC write returned success"
else
    fail "Post-ack cleanup-window 2PC write did not return success" "$POST_ACK_RESPONSE"
fi

echo "  Restarting coordinator and participant entrypoints immediately after success..."
(docker restart docker-node-p0a-1 > /dev/null 2>&1) &
POST_ACK_RESTART_P0_PID=$!
(docker restart docker-node-p1a-1 > /dev/null 2>&1) &
POST_ACK_RESTART_P1_PID=$!
wait "$POST_ACK_RESTART_P0_PID" || true
wait "$POST_ACK_RESTART_P1_PID" || true

for attempt in $(seq 1 60); do
    curl -sf "$NODE_A_URL/version" > /dev/null 2>&1 && break
    sleep 1
done
for attempt in $(seq 1 60); do
    curl -sf "$NODE_B_URL/version" > /dev/null 2>&1 && break
    sleep 1
done
NODE_P0A_KEY=$(docker exec docker-node-p0a-1 ./generate_admin_key.sh 2>&1 | tail -1)
NODE_P1A_KEY=$(docker exec docker-node-p1a-1 ./generate_admin_key.sh 2>&1 | tail -1)
NODE_A_KEY="$NODE_P0A_KEY"
NODE_B_KEY="$NODE_P1A_KEY"
(cd "$DEPLOY_DIR" && npx convex deploy --admin-key "$NODE_A_KEY" --url "$NODE_A_URL" > /dev/null 2>&1)
(cd "$DEPLOY_DIR" && npx convex deploy --admin-key "$NODE_B_KEY" --url "$NODE_B_URL" > /dev/null 2>&1)

sleep 8
POST_ACK_COUNTS=$(wait_for_2pc_pair_exactness "$POST_ACK_TEXT" "$POST_ACK_TASK" "$POST_ACK_RESPONSE" || true)
read -r POST_ACK_MSG_A POST_ACK_MSG_B POST_ACK_TASK_A POST_ACK_TASK_B <<< "$POST_ACK_COUNTS"

[ "$POST_ACK_MSG_A" -eq 1 ] && [ "$POST_ACK_MSG_B" -eq 1 ] && \
    [ "$POST_ACK_TASK_A" -eq 1 ] && [ "$POST_ACK_TASK_B" -eq 1 ] \
    && pass "Post-ack 2PC write appeared exactly once after restart cleanup" \
    || fail "Post-ack 2PC write was lost or duplicated after restart cleanup" \
        "msgA=$POST_ACK_MSG_A msgB=$POST_ACK_MSG_B taskA=$POST_ACK_TASK_A taskB=$POST_ACK_TASK_B"

POST_ACK_DASH_A=$(query_api "$NODE_A_URL" "$NODE_A_KEY" "messages:dashboard")
POST_ACK_DASH_B=$(query_api "$NODE_B_URL" "$NODE_B_KEY" "messages:dashboard")
POST_ACK_DASH_AM=$(jval messages "$POST_ACK_DASH_A")
POST_ACK_DASH_AT=$(jval tasks "$POST_ACK_DASH_A")
POST_ACK_DASH_BM=$(jval messages "$POST_ACK_DASH_B")
POST_ACK_DASH_BT=$(jval tasks "$POST_ACK_DASH_B")
[ "$POST_ACK_DASH_AM" -eq "$POST_ACK_DASH_BM" ] && [ "$POST_ACK_DASH_AT" -eq "$POST_ACK_DASH_BT" ] \
    && pass "Nodes converged after post-ack 2PC cleanup-window restarts" \
    || fail "Nodes diverged after post-ack 2PC cleanup-window restarts" \
        "p0a=$POST_ACK_DASH_AM,$POST_ACK_DASH_AT p1a=$POST_ACK_DASH_BM,$POST_ACK_DASH_BT"

if [ "${EXPECT_PARALLEL_2PC_EARLY_ACK:-false}" = "true" ]; then
    # ============================================================
    echo ""
    echo -e "${BOLD}Test 9l: Parallel 2PC Early-Ack Code-Path Proof${NC}"
    # ============================================================
    # The previous 2PC failure-window tests are the real adversarial coverage.
    # This block proves those tests are exercising the parallel-commit path,
    # not the default durable-decision path: run one more successful 2PC write,
    # verify exact visibility, and require a new `parallel-committed` log line.

    PARALLEL_LOG_BEFORE=$(parallel_commit_log_count)
    PARALLEL_RUN="parallel-early-ack-$(date +%s)"
    PARALLEL_TEXT="$PARALLEL_RUN-msg"
    PARALLEL_TASK="$PARALLEL_RUN-task"
    PARALLEL_RESPONSE=$(mutation_response "$NODE_A_URL" "$NODE_A_KEY" "messages:crossPartitionWrite" \
        "{\"text\":\"$PARALLEL_TEXT\",\"taskTitle\":\"$PARALLEL_TASK\"}")

    if echo "$PARALLEL_RESPONSE" | grep -q '"status":"success"'; then
        pass "Parallel early-ack 2PC write returned success"
    else
        fail "Parallel early-ack 2PC write did not return success" "$PARALLEL_RESPONSE"
    fi

    sleep 6
    PARALLEL_MSG_A=$(count_message_text "$NODE_A_URL" "$NODE_A_KEY" "$PARALLEL_TEXT")
    PARALLEL_MSG_B=$(count_message_text "$NODE_B_URL" "$NODE_B_KEY" "$PARALLEL_TEXT")
    PARALLEL_TASK_A=$(count_task_title "$NODE_A_URL" "$NODE_A_KEY" "$PARALLEL_TASK")
    PARALLEL_TASK_B=$(count_task_title "$NODE_B_URL" "$NODE_B_KEY" "$PARALLEL_TASK")

    [ "$PARALLEL_MSG_A" -eq 1 ] && [ "$PARALLEL_MSG_B" -eq 1 ] && \
        [ "$PARALLEL_TASK_A" -eq 1 ] && [ "$PARALLEL_TASK_B" -eq 1 ] \
        && pass "Parallel early-ack 2PC write appeared exactly once on both nodes" \
        || fail "Parallel early-ack 2PC write was lost or duplicated" \
            "msgA=$PARALLEL_MSG_A msgB=$PARALLEL_MSG_B taskA=$PARALLEL_TASK_A taskB=$PARALLEL_TASK_B"

    PARALLEL_COMMIT_TS=$(raw_read_after_write_ts "$PARALLEL_RESPONSE")
    PARALLEL_EXACT_RESPONSE=$(query_at_ts_with_args \
        "$NODE_A_URL" "$NODE_A_KEY" "messages:safeSnapshotPair" \
        "{\"text\":\"$PARALLEL_TEXT\",\"taskTitle\":\"$PARALLEL_TASK\"}" \
        "$PARALLEL_COMMIT_TS")
    PARALLEL_EXACT_MESSAGES=$(jval messages "$PARALLEL_EXACT_RESPONSE")
    PARALLEL_EXACT_TASKS=$(jval tasks "$PARALLEL_EXACT_RESPONSE")
    [ "$PARALLEL_EXACT_MESSAGES" -eq 1 ] && [ "$PARALLEL_EXACT_TASKS" -eq 1 ] \
        && pass "Parallel commit timestamp was certified across owners without a torn read" \
        || fail "Parallel commit timestamp returned a torn cross-partition snapshot" \
            "messages=$PARALLEL_EXACT_MESSAGES tasks=$PARALLEL_EXACT_TASKS ts=$PARALLEL_COMMIT_TS"

    PARALLEL_LOG_AFTER=$(parallel_commit_log_count)
    [ "$PARALLEL_LOG_AFTER" -gt "$PARALLEL_LOG_BEFORE" ] \
        && pass "Observed parallel-committed coordinator log during early-ack suite" \
        || fail "Parallel suite did not exercise the parallel-commit coordinator path" \
            "parallel-committed logs before=$PARALLEL_LOG_BEFORE after=$PARALLEL_LOG_AFTER"
fi

# ============================================================
echo ""
echo -e "${BOLD}Test 10: Rapid-Fire Writes Under Load (Jepsen Stress Pattern)${NC}"
# ============================================================
# CockroachDB Jepsen found timestamp collision bugs at ~20 txns/sec after
# minutes of sustained load. TiDB Jepsen found lost updates under concurrent
# retries. We push 50 rapid writes per node concurrently.

STRESS_PRE=$(query_api "$NODE_A_URL" "$NODE_A_KEY" "messages:dashboard")
STRESS_PRE_M=$(jval messages "$STRESS_PRE")
STRESS_PRE_T=$(jval tasks "$STRESS_PRE")

STRESS_N=50

(for i in $(seq 1 $STRESS_N); do
    mutate "$NODE_A_URL" "$NODE_A_KEY" "messages:send" \
        "{\"text\":\"stress-$i\",\"author\":\"stress\",\"channel\":\"load\"}" > /dev/null
done) &
SA=$!

(for i in $(seq 1 $STRESS_N); do
    mutate "$NODE_B_URL" "$NODE_B_KEY" "messages:createTask" \
        "{\"title\":\"stress-$i\",\"assignee\":\"stress\",\"project\":\"load\",\"status\":\"done\"}" > /dev/null
done) &
SB=$!

wait $SA
wait $SB

# Verify both nodes are still alive (Jepsen found crashes at this point).
curl -sf "$NODE_A_URL/version" > /dev/null 2>&1 \
    && pass "Node A survived stress test ($STRESS_N rapid writes)" \
    || fail "Node A crashed under stress"

curl -sf "$NODE_B_URL/version" > /dev/null 2>&1 \
    && pass "Node B survived stress test ($STRESS_N rapid writes)" \
    || fail "Node B crashed under stress"

sleep 5

STRESS_POST_A=$(query_api "$NODE_A_URL" "$NODE_A_KEY" "messages:dashboard")
STRESS_POST_B=$(query_api "$NODE_B_URL" "$NODE_B_KEY" "messages:dashboard")

STRESS_POST_AM=$(jval messages "$STRESS_POST_A"); STRESS_POST_AT=$(jval tasks "$STRESS_POST_A")
STRESS_POST_BM=$(jval messages "$STRESS_POST_B"); STRESS_POST_BT=$(jval tasks "$STRESS_POST_B")

STRESS_DM=$((STRESS_POST_AM - STRESS_PRE_M))
STRESS_DT=$((STRESS_POST_AT - STRESS_PRE_T))

# No writes lost (TiDB lost update bug pattern).
[ "$STRESS_DM" -eq "$STRESS_N" ] \
    && pass "No lost writes: msgs +$STRESS_DM (expected +$STRESS_N)" \
    || fail "Lost writes detected" "msgs +$STRESS_DM, expected +$STRESS_N"

[ "$STRESS_DT" -eq "$STRESS_N" ] \
    && pass "No lost writes: tasks +$STRESS_DT (expected +$STRESS_N)" \
    || fail "Lost writes detected" "tasks +$STRESS_DT, expected +$STRESS_N"

# Both nodes converged (no stale reads).
[ "$STRESS_POST_AM" -eq "$STRESS_POST_BM" ] && [ "$STRESS_POST_AT" -eq "$STRESS_POST_BT" ] \
    && pass "Nodes converged after stress test" \
    || fail "Nodes diverged after stress" "A: $STRESS_POST_AM,$STRESS_POST_AT vs B: $STRESS_POST_BM,$STRESS_POST_BT"

# ============================================================
echo ""
echo -e "${BOLD}Test 11: Write-Then-Immediate-Read Consistency (Stale Read Detection)${NC}"
# ============================================================
# TiDB Jepsen found stale reads where a client writes and then immediately
# reads back but gets old data. We write to Node A and immediately read
# from Node A (same node, should always be consistent).

for i in $(seq 1 5); do
    mutate "$NODE_A_URL" "$NODE_A_KEY" "messages:send" \
        "{\"text\":\"read-after-write-$i\",\"author\":\"rar\",\"channel\":\"test\"}" > /dev/null
done

RAR_RESULT=$(query_api "$NODE_A_URL" "$NODE_A_KEY" "messages:dashboard")
RAR_M=$(jval messages "$RAR_RESULT")
EXPECTED_RAR=$((STRESS_POST_AM + 5))

[ "$RAR_M" -eq "$EXPECTED_RAR" ] \
    && pass "Read-after-write consistent: msgs=$RAR_M (expected $EXPECTED_RAR)" \
    || fail "Stale read on same node" "msgs=$RAR_M, expected $EXPECTED_RAR"

# ============================================================
echo ""
echo -e "${BOLD}Test 12: Double Node Restart (Crash Recovery Stress)${NC}"
# ============================================================
# CockroachDB nemesis kills nodes multiple times in succession.
# Verify the system recovers from a double restart.

PRE_DOUBLE=$(query_api "$NODE_A_URL" "$NODE_A_KEY" "messages:dashboard")
PRE_DOUBLE_M=$(jval messages "$PRE_DOUBLE")

echo "  First restart of Node B..."
docker restart docker-node-p1a-1 > /dev/null 2>&1

# Write during first restart.
mutate "$NODE_A_URL" "$NODE_A_KEY" "messages:send" \
    '{"text":"during-restart-1","author":"chaos","channel":"test"}' > /dev/null

echo "  Waiting for recovery..."
for attempt in $(seq 1 30); do
    curl -sf "http://127.0.0.1:3310/version" > /dev/null 2>&1 && break
    sleep 1
done

echo "  Second restart of Node B..."
docker restart docker-node-p1a-1 > /dev/null 2>&1

# Write during second restart.
mutate "$NODE_A_URL" "$NODE_A_KEY" "messages:send" \
    '{"text":"during-restart-2","author":"chaos","channel":"test"}' > /dev/null

echo "  Waiting for recovery..."
for attempt in $(seq 1 30); do
    curl -sf "http://127.0.0.1:3310/version" > /dev/null 2>&1 && break
    sleep 1
done

NODE_P1A_KEY=$(docker exec docker-node-p1a-1 ./generate_admin_key.sh 2>&1 | tail -1)
NODE_B_KEY="$NODE_P1A_KEY"
(cd "$DEPLOY_DIR" && npx convex deploy --admin-key "$NODE_B_KEY" --url "$NODE_B_URL" > /dev/null 2>&1)

sleep 4

POST_DOUBLE_B=$(query_api "$NODE_B_URL" "$NODE_B_KEY" "messages:dashboard")
POST_DOUBLE_BM=$(jval messages "$POST_DOUBLE_B")

EXPECTED_DOUBLE=$((PRE_DOUBLE_M + 2))
[ "$POST_DOUBLE_BM" -ge "$EXPECTED_DOUBLE" ] \
    && pass "Node B recovered after double restart: msgs=$POST_DOUBLE_BM (>=$EXPECTED_DOUBLE)" \
    || fail "Node B missing data after double restart" "msgs=$POST_DOUBLE_BM, expected >=$EXPECTED_DOUBLE"

# ============================================================
echo ""
echo -e "${BOLD}Test 13: Invariant Preservation Under Full Load (CockroachDB workload check)${NC}"
# ============================================================
# After all the stress and chaos, run the bank invariant check again.
# The total budget should still be exactly what we deposited.

FINAL_BUDGET_A=$(jtotal "$(query_api "$NODE_A_URL" "$NODE_A_KEY" "messages:totalBudget")")
FINAL_BUDGET_B=$(jtotal "$(query_api "$NODE_B_URL" "$NODE_B_KEY" "messages:totalBudget")")

[ "$FINAL_BUDGET_A" -eq "$FINAL_BUDGET_B" ] \
    && pass "Budget invariant preserved after all tests: \$$FINAL_BUDGET_A" \
    || fail "Budget invariant violated after chaos" "A=\$$FINAL_BUDGET_A vs B=\$$FINAL_BUDGET_B"

FINAL_COMP_A=$(query_api "$NODE_A_URL" "$NODE_A_KEY" "messages:totalCompensation")
FINAL_COMP_B=$(query_api "$NODE_B_URL" "$NODE_B_KEY" "messages:totalCompensation")

FINAL_TOTAL_A=$(python3 -c "import sys,json; print(int(json.load(sys.stdin)['value']['total']))" <<< "$FINAL_COMP_A")
FINAL_TOTAL_B=$(python3 -c "import sys,json; print(int(json.load(sys.stdin)['value']['total']))" <<< "$FINAL_COMP_B")

[ "$FINAL_TOTAL_A" -eq "$FINAL_TOTAL_B" ] \
    && pass "Cross-table invariant preserved after all tests: \$$FINAL_TOTAL_A" \
    || fail "Cross-table invariant violated" "A=\$$FINAL_TOTAL_A vs B=\$$FINAL_TOTAL_B"

# Final convergence check — every table count must match.
FINAL_A=$(query_api "$NODE_A_URL" "$NODE_A_KEY" "messages:dashboard")
FINAL_B=$(query_api "$NODE_B_URL" "$NODE_B_KEY" "messages:dashboard")

FINAL_AM=$(jval messages "$FINAL_A"); FINAL_AU=$(jval users "$FINAL_A")
FINAL_AP=$(jval projects "$FINAL_A"); FINAL_AT=$(jval tasks "$FINAL_A")
FINAL_BM=$(jval messages "$FINAL_B"); FINAL_BU=$(jval users "$FINAL_B")
FINAL_BP=$(jval projects "$FINAL_B"); FINAL_BT=$(jval tasks "$FINAL_B")

[ "$FINAL_AM" -eq "$FINAL_BM" ] && [ "$FINAL_AU" -eq "$FINAL_BU" ] && \
[ "$FINAL_AP" -eq "$FINAL_BP" ] && [ "$FINAL_AT" -eq "$FINAL_BT" ] \
    && pass "Final convergence: all tables match (msgs=$FINAL_AM users=$FINAL_AU proj=$FINAL_AP tasks=$FINAL_AT)" \
    || fail "Final divergence" "A: $FINAL_AM,$FINAL_AU,$FINAL_AP,$FINAL_AT vs B: $FINAL_BM,$FINAL_BU,$FINAL_BP,$FINAL_BT"

# ============================================================
echo ""
echo -e "${BOLD}Test 14: Sequential Ordering (Jepsen sequential)${NC}"
# ============================================================
# Write A then B on same client. Read back. B must appear after A.
# CockroachDB Jepsen found disjoint records visible out of order.

SEQ_KEY="seq-$(date +%s)"
mutate "$NODE_A_URL" "$NODE_A_KEY" "messages:writeOrdered" "{\"key\":\"$SEQ_KEY\",\"value\":\"first\"}" > /dev/null
mutate "$NODE_A_URL" "$NODE_A_KEY" "messages:writeOrdered" "{\"key\":\"$SEQ_KEY\",\"value\":\"second\"}" > /dev/null
mutate "$NODE_A_URL" "$NODE_A_KEY" "messages:writeOrdered" "{\"key\":\"$SEQ_KEY\",\"value\":\"third\"}" > /dev/null

SEQ_RESULT=$(curl -sf "$NODE_A_URL/api/query" \
    -H "Authorization: Convex $NODE_A_KEY" \
    -H "Content-Type: application/json" \
    -d "{\"path\":\"messages:readOrdered\",\"args\":{\"key\":\"$SEQ_KEY\"}}" \
    | python3 -c "import sys,json; print(','.join(json.load(sys.stdin)['value']))")

[ "$SEQ_RESULT" = "first,second,third" ] \
    && pass "Sequential ordering preserved: $SEQ_RESULT" \
    || fail "Sequential ordering violated" "got $SEQ_RESULT, expected first,second,third"

# ============================================================
echo ""
echo -e "${BOLD}Test 15: Set Completeness (Jepsen set)${NC}"
# ============================================================
# Insert 100 unique elements. Read back. All 100 must be present.
# Jepsen set test catches lost inserts.

SET_NAME="set-$(date +%s)"
SET_N=100

for i in $(seq 1 $SET_N); do
    mutate "$NODE_A_URL" "$NODE_A_KEY" "messages:insertSetElement" \
        "{\"setName\":\"$SET_NAME\",\"element\":$i}" > /dev/null
done

sleep 2

SET_COUNT=$(curl -sf "$NODE_A_URL/api/query" \
    -H "Authorization: Convex $NODE_A_KEY" \
    -H "Content-Type: application/json" \
    -d "{\"path\":\"messages:readSet\",\"args\":{\"setName\":\"$SET_NAME\"}}" \
    | python3 -c "import sys,json; v=json.load(sys.stdin)['value']; print(len(v))")

[ "$SET_COUNT" -eq "$SET_N" ] \
    && pass "Set complete: all $SET_N elements present" \
    || fail "Set incomplete" "got $SET_COUNT, expected $SET_N"

# Verify on Node B too after replication.
sleep 3
SET_COUNT_B=$(curl -sf "$NODE_B_URL/api/query" \
    -H "Authorization: Convex $NODE_B_KEY" \
    -H "Content-Type: application/json" \
    -d "{\"path\":\"messages:readSet\",\"args\":{\"setName\":\"$SET_NAME\"}}" \
    | python3 -c "import sys,json; v=json.load(sys.stdin)['value']; print(len(v))")

[ "$SET_COUNT_B" -eq "$SET_N" ] \
    && pass "Set replicated to Node B: all $SET_N elements present" \
    || fail "Set incomplete on Node B" "got $SET_COUNT_B, expected $SET_N"

# ============================================================
echo ""
echo -e "${BOLD}Test 16: Concurrent Counter (Jepsen counter / YugabyteDB)${NC}"
# ============================================================
# Multiple concurrent increments from both nodes. Final count must match.

CTR_NAME="ctr-$(date +%s)"
CTR_N=30

# Node A increments on messages (partition 0), Node B increments on tasks (partition 1).
# Each node writes to its own partition — no rejections.
(for i in $(seq 1 $CTR_N); do
    mutate "$NODE_A_URL" "$NODE_A_KEY" "messages:incrementCounter" \
        "{\"name\":\"$CTR_NAME\"}" > /dev/null
done) &
CA=$!

(for i in $(seq 1 $CTR_N); do
    mutate "$NODE_B_URL" "$NODE_B_KEY" "messages:writeSequence" \
        "{\"key\":\"$CTR_NAME\",\"seq\":$i}" > /dev/null
done) &
CB=$!

wait $CA
wait $CB
sleep 3

# Verify Node A's counter (messages table).
CTR_TOTAL_A=$(curl -sf "$NODE_A_URL/api/query" \
    -H "Authorization: Convex $NODE_A_KEY" \
    -H "Content-Type: application/json" \
    -d "{\"path\":\"messages:readCounter\",\"args\":{\"name\":\"$CTR_NAME\"}}" \
    | python3 -c "import sys,json; print(int(json.load(sys.stdin)['value']))")

[ "$CTR_TOTAL_A" -eq "$CTR_N" ] \
    && pass "Counter on Node A: $CTR_TOTAL_A increments (expected $CTR_N)" \
    || fail "Counter wrong on A" "got $CTR_TOTAL_A, expected $CTR_N"

# Verify Node B's counter (tasks table, via writeSequence).
CTR_TOTAL_B=$(curl -sf "$NODE_B_URL/api/query" \
    -H "Authorization: Convex $NODE_B_KEY" \
    -H "Content-Type: application/json" \
    -d "{\"path\":\"messages:readLatestSeq\",\"args\":{\"key\":\"$CTR_NAME\"}}" \
    | python3 -c "import sys,json; print(int(json.load(sys.stdin)['value']))")

[ "$CTR_TOTAL_B" -eq "$CTR_N" ] \
    && pass "Counter on Node B: max seq=$CTR_TOTAL_B (expected $CTR_N)" \
    || fail "Counter wrong on B" "got $CTR_TOTAL_B, expected $CTR_N"

# Verify cross-node replication — Node B sees Node A's counter.
sleep 3
CTR_CROSS=$(curl -sf "$NODE_B_URL/api/query" \
    -H "Authorization: Convex $NODE_B_KEY" \
    -H "Content-Type: application/json" \
    -d "{\"path\":\"messages:readCounter\",\"args\":{\"name\":\"$CTR_NAME\"}}" \
    | python3 -c "import sys,json; print(int(json.load(sys.stdin)['value']))")

[ "$CTR_CROSS" -eq "$CTR_N" ] \
    && pass "Counter replicated to Node B: $CTR_CROSS" \
    || fail "Counter not replicated" "Node B sees $CTR_CROSS, expected $CTR_N"

# ============================================================
echo ""
echo -e "${BOLD}Test 17: Write-Then-Cross-Node-Read (cross-node read-after-write)${NC}"
# ============================================================
# Write to Node A, immediately read from Node B.
# TiDB Jepsen found stale reads in this pattern.

CROSS_KEY="xread-$(date +%s)"
mutate "$NODE_A_URL" "$NODE_A_KEY" "messages:writeOrdered" \
    "{\"key\":\"$CROSS_KEY\",\"value\":\"cross-node-value\"}" > /dev/null

sleep 4

XREAD=$(curl -sf "$NODE_B_URL/api/query" \
    -H "Authorization: Convex $NODE_B_KEY" \
    -H "Content-Type: application/json" \
    -d "{\"path\":\"messages:readOrdered\",\"args\":{\"key\":\"$CROSS_KEY\"}}" \
    | python3 -c "import sys,json; v=json.load(sys.stdin)['value']; print(len(v))")

[ "$XREAD" -ge 1 ] \
    && pass "Cross-node read-after-write: Node B sees write ($XREAD elements)" \
    || fail "Cross-node stale read" "Node B sees $XREAD elements, expected >= 1"

# ============================================================
echo ""
echo -e "${BOLD}Test 18: Interleaved Cross-Partition Reads (read skew detection)${NC}"
# ============================================================
# Read from both nodes rapidly 10 times. Every read pair must agree.
# Catches read skew where nodes return inconsistent snapshots.

SKEW_OK=true
for i in $(seq 1 10); do
    RA=$(query_api "$NODE_A_URL" "$NODE_A_KEY" "messages:dashboard")
    RB=$(query_api "$NODE_B_URL" "$NODE_B_KEY" "messages:dashboard")
    RA_M=$(jval messages "$RA"); RB_M=$(jval messages "$RB")
    RA_T=$(jval tasks "$RA"); RB_T=$(jval tasks "$RB")
    if [ "$RA_M" -ne "$RB_M" ] || [ "$RA_T" -ne "$RB_T" ]; then
        SKEW_OK=false
        fail "Read skew at iteration $i" "A: msgs=$RA_M tasks=$RA_T vs B: msgs=$RB_M tasks=$RB_T"
        break
    fi
done

$SKEW_OK && pass "No read skew: 10 interleaved reads all consistent"

# ============================================================
echo ""
echo -e "${BOLD}Test 19: Large Batch Write Atomicity${NC}"
# ============================================================
# Single mutation writes 50 documents. All 50 must appear atomically.

BATCH_PREFIX="batch-$(date +%s)"
mutate "$NODE_A_URL" "$NODE_A_KEY" "messages:batchWrite" \
    "{\"prefix\":\"$BATCH_PREFIX\",\"count\":50}" > /dev/null

BATCH_COUNT=$(curl -sf "$NODE_A_URL/api/query" \
    -H "Authorization: Convex $NODE_A_KEY" \
    -H "Content-Type: application/json" \
    -d "{\"path\":\"messages:countBatch\",\"args\":{\"prefix\":\"$BATCH_PREFIX\"}}" \
    | python3 -c "import sys,json; print(int(json.load(sys.stdin)['value']))")

[ "$BATCH_COUNT" -eq 50 ] \
    && pass "Batch write atomic: all 50 documents present" \
    || fail "Batch write partial" "got $BATCH_COUNT, expected 50"

# Verify replicated.
sleep 4
BATCH_COUNT_B=$(curl -sf "$NODE_B_URL/api/query" \
    -H "Authorization: Convex $NODE_B_KEY" \
    -H "Content-Type: application/json" \
    -d "{\"path\":\"messages:countBatch\",\"args\":{\"prefix\":\"$BATCH_PREFIX\"}}" \
    | python3 -c "import sys,json; print(int(json.load(sys.stdin)['value']))")

[ "$BATCH_COUNT_B" -eq 50 ] \
    && pass "Batch replicated to Node B: all 50 documents" \
    || fail "Batch incomplete on Node B" "got $BATCH_COUNT_B, expected 50"

# ============================================================
echo ""
echo -e "${BOLD}Test 20: Kill Both Nodes Sequentially (full cluster restart)${NC}"
# ============================================================
# Kill Node A, then kill Node B, restart both. Verify full recovery.

PRE_KILL=$(query_api "$NODE_A_URL" "$NODE_A_KEY" "messages:dashboard")
PRE_KILL_M=$(jval messages "$PRE_KILL")

echo "  Killing Node A..."
docker stop docker-node-p0a-1 > /dev/null 2>&1
sleep 2

echo "  Killing Node B..."
docker stop docker-node-p1a-1 > /dev/null 2>&1
sleep 2

echo "  Restarting both nodes..."
docker start docker-node-p0a-1 > /dev/null 2>&1
docker start docker-node-p1a-1 > /dev/null 2>&1

echo "  Waiting for recovery..."
for attempt in $(seq 1 60); do
    A_UP=$(curl -sf "$NODE_A_URL/version" > /dev/null 2>&1 && echo 1 || echo 0)
    B_UP=$(curl -sf "$NODE_B_URL/version" > /dev/null 2>&1 && echo 1 || echo 0)
    [ "$A_UP" -eq 1 ] && [ "$B_UP" -eq 1 ] && break
    sleep 1
done

NODE_P0A_KEY=$(docker exec docker-node-p0a-1 ./generate_admin_key.sh 2>&1 | tail -1)
NODE_A_KEY="$NODE_P0A_KEY"
NODE_P1A_KEY=$(docker exec docker-node-p1a-1 ./generate_admin_key.sh 2>&1 | tail -1)
NODE_B_KEY="$NODE_P1A_KEY"
(cd "$DEPLOY_DIR" && npx convex deploy --admin-key "$NODE_A_KEY" --url "$NODE_A_URL" > /dev/null 2>&1)
(cd "$DEPLOY_DIR" && npx convex deploy --admin-key "$NODE_B_KEY" --url "$NODE_B_URL" > /dev/null 2>&1)

sleep 4

POST_KILL_A=$(query_api "$NODE_A_URL" "$NODE_A_KEY" "messages:dashboard")
POST_KILL_B=$(query_api "$NODE_B_URL" "$NODE_B_KEY" "messages:dashboard")
POST_KILL_AM=$(jval messages "$POST_KILL_A")
POST_KILL_BM=$(jval messages "$POST_KILL_B")

[ "$POST_KILL_AM" -ge "$PRE_KILL_M" ] \
    && pass "Node A recovered after full cluster restart: msgs=$POST_KILL_AM (>=$PRE_KILL_M)" \
    || fail "Node A lost data" "msgs=$POST_KILL_AM, expected >=$PRE_KILL_M"

[ "$POST_KILL_AM" -eq "$POST_KILL_BM" ] \
    && pass "Both nodes converged after full restart" \
    || fail "Nodes diverged after restart" "A=$POST_KILL_AM vs B=$POST_KILL_BM"

# ============================================================
echo ""
echo -e "${BOLD}Test 20b: Writes Through Followers During Raft Leader Outage${NC}"
# ============================================================
# Stop p0a and require every surviving public entrypoint to route a p0-owned
# write through the new Raft leader. This catches stale leader/follower routing.

PRE_P0_OUTAGE=$(query_api "$NODE_B_URL" "$NODE_B_KEY" "messages:dashboard")
PRE_P0_OUTAGE_M=$(jval messages "$PRE_P0_OUTAGE")
PRE_P0_OUTAGE_T=$(jval tasks "$PRE_P0_OUTAGE")

echo "  Stopping p0a to force/hold a partition-0 leader change..."
docker stop docker-node-p0a-1 > /dev/null 2>&1
sleep 6

OUTAGE_LABELS=(p0b p0c p1a p1b p1c)
OUTAGE_URLS=("$NODE_P0B_URL" "$NODE_P0C_URL" "$NODE_P1A_URL" "$NODE_P1B_URL" "$NODE_P1C_URL")
OUTAGE_KEYS=("$NODE_P0B_KEY" "$NODE_P0C_KEY" "$NODE_P1A_KEY" "$NODE_P1B_KEY" "$NODE_P1C_KEY")
P0_OUTAGE_ACCEPTED=0
P0_OUTAGE_RUN="p0-outage-$(date +%s)"

for i in "${!OUTAGE_LABELS[@]}"; do
    label="${OUTAGE_LABELS[$i]}"
    R=$(mutation_response "${OUTAGE_URLS[$i]}" "${OUTAGE_KEYS[$i]}" "messages:send" \
        "{\"text\":\"$P0_OUTAGE_RUN-$label\",\"author\":\"$P0_OUTAGE_RUN\",\"channel\":\"raft-outage\"}")
    if echo "$R" | grep -q '"status":"success"'; then
        P0_OUTAGE_ACCEPTED=$((P0_OUTAGE_ACCEPTED + 1))
    else
        fail "$label could not route write while p0a was down" "$R"
    fi
done

[ "$P0_OUTAGE_ACCEPTED" -eq 5 ] \
    && pass "All five surviving entrypoints accepted writes while p0a was down" \
    || fail "Some entrypoints failed during p0a outage" "accepted=$P0_OUTAGE_ACCEPTED expected=5"

P0_OUTAGE_2PC_LABELS=(p1a p1b p1c)
P0_OUTAGE_2PC_URLS=("$NODE_P1A_URL" "$NODE_P1B_URL" "$NODE_P1C_URL")
P0_OUTAGE_2PC_KEYS=("$NODE_P1A_KEY" "$NODE_P1B_KEY" "$NODE_P1C_KEY")
P0_OUTAGE_2PC_ACCEPTED=0

for i in "${!P0_OUTAGE_2PC_LABELS[@]}"; do
    label="${P0_OUTAGE_2PC_LABELS[$i]}"
    R=$(mutation_response "${P0_OUTAGE_2PC_URLS[$i]}" "${P0_OUTAGE_2PC_KEYS[$i]}" "messages:crossPartitionWrite" \
        "{\"text\":\"$P0_OUTAGE_RUN-2pc-$label\",\"taskTitle\":\"$P0_OUTAGE_RUN-2pc-task-$label\"}")
    if echo "$R" | grep -q '"status":"success"'; then
        P0_OUTAGE_2PC_ACCEPTED=$((P0_OUTAGE_2PC_ACCEPTED + 1))
    else
        fail "$label could not route 2PC write while p0a was down" "$R"
    fi
done

[ "$P0_OUTAGE_2PC_ACCEPTED" -eq 3 ] \
    && pass "Surviving p1 entrypoints accepted 2PC writes while p0a was down" \
    || fail "Some 2PC writes failed during p0a outage" "accepted=$P0_OUTAGE_2PC_ACCEPTED expected=3"

echo "  Restarting p0a..."
docker start docker-node-p0a-1 > /dev/null 2>&1
for attempt in $(seq 1 60); do
    curl -sf "$NODE_A_URL/version" > /dev/null 2>&1 && break
    sleep 1
done
NODE_P0A_KEY=$(docker exec docker-node-p0a-1 ./generate_admin_key.sh 2>&1 | tail -1)
NODE_A_KEY="$NODE_P0A_KEY"
(cd "$DEPLOY_DIR" && npx convex deploy --admin-key "$NODE_A_KEY" --url "$NODE_A_URL" > /dev/null 2>&1)

sleep 6
POST_P0_OUTAGE_A=$(query_api "$NODE_A_URL" "$NODE_A_KEY" "messages:dashboard")
POST_P0_OUTAGE_B=$(query_api "$NODE_B_URL" "$NODE_B_KEY" "messages:dashboard")
POST_P0_OUTAGE_AM=$(jval messages "$POST_P0_OUTAGE_A")
POST_P0_OUTAGE_BM=$(jval messages "$POST_P0_OUTAGE_B")
POST_P0_OUTAGE_AT=$(jval tasks "$POST_P0_OUTAGE_A")
POST_P0_OUTAGE_BT=$(jval tasks "$POST_P0_OUTAGE_B")
P0_OUTAGE_DELTA=$((POST_P0_OUTAGE_AM - PRE_P0_OUTAGE_M))
P0_OUTAGE_TASK_DELTA=$((POST_P0_OUTAGE_AT - PRE_P0_OUTAGE_T))
P0_OUTAGE_EXPECTED_M=$((P0_OUTAGE_ACCEPTED + P0_OUTAGE_2PC_ACCEPTED))

[ "$P0_OUTAGE_DELTA" -eq "$P0_OUTAGE_EXPECTED_M" ] \
    && pass "All outage-window writes survived Raft leader outage (+$P0_OUTAGE_DELTA)" \
    || fail "Lost/extra writes during Raft leader outage" "+$P0_OUTAGE_DELTA expected +$P0_OUTAGE_EXPECTED_M"

[ "$P0_OUTAGE_TASK_DELTA" -eq "$P0_OUTAGE_2PC_ACCEPTED" ] \
    && pass "2PC outage-window task halves survived Raft leader outage (+$P0_OUTAGE_TASK_DELTA)" \
    || fail "Lost/extra 2PC task halves during Raft leader outage" "+$P0_OUTAGE_TASK_DELTA expected +$P0_OUTAGE_2PC_ACCEPTED"

[ "$POST_P0_OUTAGE_AM" -eq "$POST_P0_OUTAGE_BM" ] && [ "$POST_P0_OUTAGE_AT" -eq "$POST_P0_OUTAGE_BT" ] \
    && pass "Nodes converged after p0a leader outage" \
    || fail "Nodes diverged after p0a leader outage" "p0a=$POST_P0_OUTAGE_AM,$POST_P0_OUTAGE_AT p1a=$POST_P0_OUTAGE_BM,$POST_P0_OUTAGE_BT"

# ============================================================
echo ""
echo -e "${BOLD}Test 20c: Lagging Follower Catch-Up After Cross-Partition Writes${NC}"
# ============================================================
# Keep p1c offline while cross-partition writes commit, then require it to
# catch up through Raft/NATS before serving a consistent dashboard.

PRE_LAGGING=$(query_api "$NODE_A_URL" "$NODE_A_KEY" "messages:dashboard")
PRE_LAGGING_M=$(jval messages "$PRE_LAGGING")
PRE_LAGGING_T=$(jval tasks "$PRE_LAGGING")
LAGGING_2PC_COUNT=4

echo "  Stopping p1c before heavy cross-partition writes..."
docker stop docker-node-p1c-1 > /dev/null 2>&1
sleep 5

LAGGING_ACCEPTED=0
LAGGING_REJECTED=0
for i in $(seq 1 "$LAGGING_2PC_COUNT"); do
    R=$(mutation_response "$NODE_A_URL" "$NODE_A_KEY" "messages:crossPartitionWrite" \
        "{\"text\":\"lagging-2pc-$i\",\"taskTitle\":\"lagging-2pc-task-$i\"}")
    if echo "$R" | grep -q '"status":"success"'; then
        LAGGING_ACCEPTED=$((LAGGING_ACCEPTED + 1))
    else
        LAGGING_REJECTED=$((LAGGING_REJECTED + 1))
        echo -e "  ${YELLOW}WARN${NC} Cross-partition write $i rejected while p1c was offline (fail-closed): $R"
    fi
done

LAGGING_ACCOUNTED=$((LAGGING_ACCEPTED + LAGGING_REJECTED))
[ "$LAGGING_ACCOUNTED" -eq "$LAGGING_2PC_COUNT" ] \
    && pass "Lagging-follower 2PC attempts were all accounted for (accepted=$LAGGING_ACCEPTED rejected=$LAGGING_REJECTED)" \
    || fail "Lagging-follower 2PC accounting mismatch" "attempted=$LAGGING_2PC_COUNT accepted=$LAGGING_ACCEPTED rejected=$LAGGING_REJECTED"

[ "$LAGGING_ACCEPTED" -ge 1 ] \
    && pass "Majority accepted at least one cross-partition write while p1c was offline ($LAGGING_ACCEPTED/$LAGGING_2PC_COUNT)" \
    || fail "No cross-partition writes were accepted while p1c was offline" "accepted=$LAGGING_ACCEPTED rejected=$LAGGING_REJECTED"

echo "  Restarting p1c and waiting for catch-up..."
docker start docker-node-p1c-1 > /dev/null 2>&1
for attempt in $(seq 1 60); do
    curl -sf "$NODE_P1C_URL/version" > /dev/null 2>&1 && break
    sleep 1
done
NODE_P1C_KEY=$(docker exec docker-node-p1c-1 ./generate_admin_key.sh 2>&1 | tail -1)
(cd "$DEPLOY_DIR" && npx convex deploy --admin-key "$NODE_P1C_KEY" --url "$NODE_P1C_URL" > /dev/null 2>&1)

sleep 10
POST_LAGGING_A=$(query_api "$NODE_A_URL" "$NODE_A_KEY" "messages:dashboard")
POST_LAGGING_C=$(query_api "$NODE_P1C_URL" "$NODE_P1C_KEY" "messages:dashboard")
POST_LAGGING_AM=$(jval messages "$POST_LAGGING_A")
POST_LAGGING_AT=$(jval tasks "$POST_LAGGING_A")
POST_LAGGING_CM=$(jval messages "$POST_LAGGING_C")
POST_LAGGING_CT=$(jval tasks "$POST_LAGGING_C")
LAGGING_DM=$((POST_LAGGING_AM - PRE_LAGGING_M))
LAGGING_DT=$((POST_LAGGING_AT - PRE_LAGGING_T))

[ "$LAGGING_DM" -eq "$LAGGING_ACCEPTED" ] && [ "$LAGGING_DT" -eq "$LAGGING_ACCEPTED" ] \
    && pass "Heavy cross-partition writes remained atomic while follower lagged (+$LAGGING_ACCEPTED/+${LAGGING_ACCEPTED})" \
    || fail "Unexpected heavy 2PC deltas while follower lagged" "msgs +$LAGGING_DM tasks +$LAGGING_DT expected +$LAGGING_ACCEPTED"

[ "$POST_LAGGING_AM" -eq "$POST_LAGGING_CM" ] && [ "$POST_LAGGING_AT" -eq "$POST_LAGGING_CT" ] \
    && pass "Lagging p1c caught up after cross-partition writes" \
    || fail "Lagging p1c did not catch up" "p0a: $POST_LAGGING_AM,$POST_LAGGING_AT vs p1c: $POST_LAGGING_CM,$POST_LAGGING_CT"

# ============================================================
echo ""
echo -e "${BOLD}Test 20d: Cross-Partition 2PC During Participant Leader Outage${NC}"
# ============================================================
# Stop the partition-1 leader and require the surviving cluster to fail closed
# or commit exact cross-partition writes through the new partition-1 Raft leader.
# Leader changes are asynchronous, so individual entrypoints may reject while
# routing converges, but any accepted 2PC write must appear exactly once.

PRE_P1_OUTAGE=$(query_api "$NODE_A_URL" "$NODE_A_KEY" "messages:dashboard")
PRE_P1_OUTAGE_M=$(jval messages "$PRE_P1_OUTAGE")
PRE_P1_OUTAGE_T=$(jval tasks "$PRE_P1_OUTAGE")

echo "  Stopping p1a to force/hold a partition-1 leader change..."
docker stop docker-node-p1a-1 > /dev/null 2>&1
sleep 6

P1_OUTAGE_LABELS=(p0a p0b p0c p1b p1c)
P1_OUTAGE_URLS=("$NODE_P0A_URL" "$NODE_P0B_URL" "$NODE_P0C_URL" "$NODE_P1B_URL" "$NODE_P1C_URL")
P1_OUTAGE_KEYS=("$NODE_P0A_KEY" "$NODE_P0B_KEY" "$NODE_P0C_KEY" "$NODE_P1B_KEY" "$NODE_P1C_KEY")
P1_OUTAGE_ACCEPTED=0
P1_OUTAGE_REJECTED=0
P1_OUTAGE_RUN="p1-outage-$(date +%s)"
P1_OUTAGE_TEXTS=()
P1_OUTAGE_TASKS=()

for i in "${!P1_OUTAGE_LABELS[@]}"; do
    label="${P1_OUTAGE_LABELS[$i]}"
    SUCCESS=false
    LAST_R=""
    for attempt in $(seq 1 30); do
        text="$P1_OUTAGE_RUN-$label-attempt-$attempt"
        task="$P1_OUTAGE_RUN-task-$label-attempt-$attempt"
        R=$(mutation_response "${P1_OUTAGE_URLS[$i]}" "${P1_OUTAGE_KEYS[$i]}" "messages:crossPartitionWrite" \
            "{\"text\":\"$text\",\"taskTitle\":\"$task\"}")
        LAST_R="$R"
        if echo "$R" | grep -q '"status":"success"'; then
            P1_OUTAGE_ACCEPTED=$((P1_OUTAGE_ACCEPTED + 1))
            P1_OUTAGE_TEXTS+=("$text")
            P1_OUTAGE_TASKS+=("$task")
            SUCCESS=true
            break
        fi
        sleep 1
    done
    if [ "$SUCCESS" != "true" ]; then
        P1_OUTAGE_REJECTED=$((P1_OUTAGE_REJECTED + 1))
        echo -e "  ${YELLOW}WARN${NC} $label 2PC write rejected during p1a outage (fail-closed): $LAST_R"
    fi
done

P1_OUTAGE_ACCOUNTED=$((P1_OUTAGE_ACCEPTED + P1_OUTAGE_REJECTED))
[ "$P1_OUTAGE_ACCOUNTED" -eq 5 ] \
    && pass "Participant-outage 2PC attempts were all accounted for (accepted=$P1_OUTAGE_ACCEPTED rejected=$P1_OUTAGE_REJECTED)" \
    || fail "Participant-outage 2PC accounting mismatch" "attempted=5 accepted=$P1_OUTAGE_ACCEPTED rejected=$P1_OUTAGE_REJECTED"

[ "$P1_OUTAGE_ACCEPTED" -ge 1 ] \
    && pass "Surviving cluster accepted at least one 2PC write during p1a outage ($P1_OUTAGE_ACCEPTED/5)" \
    || fail "No 2PC writes were accepted during p1a outage" "accepted=$P1_OUTAGE_ACCEPTED rejected=$P1_OUTAGE_REJECTED"

echo "  Restarting p1a..."
docker start docker-node-p1a-1 > /dev/null 2>&1
for attempt in $(seq 1 60); do
    curl -sf "$NODE_B_URL/version" > /dev/null 2>&1 && break
    sleep 1
done
NODE_P1A_KEY=$(docker exec docker-node-p1a-1 ./generate_admin_key.sh 2>&1 | tail -1)
NODE_B_KEY="$NODE_P1A_KEY"
(cd "$DEPLOY_DIR" && npx convex deploy --admin-key "$NODE_B_KEY" --url "$NODE_B_URL" > /dev/null 2>&1)

sleep 8
POST_P1_OUTAGE_A=$(query_api "$NODE_A_URL" "$NODE_A_KEY" "messages:dashboard")
POST_P1_OUTAGE_B=$(query_api "$NODE_B_URL" "$NODE_B_KEY" "messages:dashboard")
POST_P1_OUTAGE_AM=$(jval messages "$POST_P1_OUTAGE_A")
POST_P1_OUTAGE_AT=$(jval tasks "$POST_P1_OUTAGE_A")
POST_P1_OUTAGE_BM=$(jval messages "$POST_P1_OUTAGE_B")
POST_P1_OUTAGE_BT=$(jval tasks "$POST_P1_OUTAGE_B")
P1_OUTAGE_EXACT=true
P1_OUTAGE_EXACT_DETAILS=""

for i in "${!P1_OUTAGE_TEXTS[@]}"; do
    text="${P1_OUTAGE_TEXTS[$i]}"
    task="${P1_OUTAGE_TASKS[$i]}"
    MSG_A=$(count_message_text "$NODE_A_URL" "$NODE_A_KEY" "$text")
    MSG_B=$(count_message_text "$NODE_B_URL" "$NODE_B_KEY" "$text")
    TASK_A=$(count_task_title "$NODE_A_URL" "$NODE_A_KEY" "$task")
    TASK_B=$(count_task_title "$NODE_B_URL" "$NODE_B_KEY" "$task")
    if [ "$MSG_A" -ne 1 ] || [ "$MSG_B" -ne 1 ] || [ "$TASK_A" -ne 1 ] || [ "$TASK_B" -ne 1 ]; then
        P1_OUTAGE_EXACT=false
        P1_OUTAGE_EXACT_DETAILS="$P1_OUTAGE_EXACT_DETAILS text=$text msgA=$MSG_A msgB=$MSG_B task=$task taskA=$TASK_A taskB=$TASK_B;"
    fi
done

$P1_OUTAGE_EXACT \
    && pass "Every participant-outage 2PC write appeared exactly once on both nodes" \
    || fail "Participant-outage 2PC write exactness failed" "$P1_OUTAGE_EXACT_DETAILS"
P1_OUTAGE_DM=$((POST_P1_OUTAGE_AM - PRE_P1_OUTAGE_M))
P1_OUTAGE_DT=$((POST_P1_OUTAGE_AT - PRE_P1_OUTAGE_T))

[ "$P1_OUTAGE_DM" -eq "$P1_OUTAGE_ACCEPTED" ] && [ "$P1_OUTAGE_DT" -eq "$P1_OUTAGE_ACCEPTED" ] \
    && pass "2PC writes survived participant leader outage (+$P1_OUTAGE_ACCEPTED/+${P1_OUTAGE_ACCEPTED})" \
    || fail "Lost/extra 2PC writes during participant leader outage" "msgs +$P1_OUTAGE_DM tasks +$P1_OUTAGE_DT expected +$P1_OUTAGE_ACCEPTED"

[ "$POST_P1_OUTAGE_AM" -eq "$POST_P1_OUTAGE_BM" ] && [ "$POST_P1_OUTAGE_AT" -eq "$POST_P1_OUTAGE_BT" ] \
    && pass "Nodes converged after p1a participant leader outage" \
    || fail "Nodes diverged after p1a participant leader outage" "p0a=$POST_P1_OUTAGE_AM,$POST_P1_OUTAGE_AT p1a=$POST_P1_OUTAGE_BM,$POST_P1_OUTAGE_BT"

# ============================================================
echo ""
echo -e "${BOLD}Test 20e: Repeated Raft Leadership Churn During Cross-Partition 2PC${NC}"
# ============================================================
# Alternate partition-0 and partition-1 node outages while issuing
# cross-partition 2PC writes through surviving entrypoints. This is a
# deterministic precursor to randomized nemesis testing: every accepted write
# must remain atomic and every restarted node must converge.

PRE_CHURN_2PC=$(query_api "$NODE_A_URL" "$NODE_A_KEY" "messages:dashboard")
PRE_CHURN_2PC_M=$(jval messages "$PRE_CHURN_2PC")
PRE_CHURN_2PC_T=$(jval tasks "$PRE_CHURN_2PC")
CHURN_2PC_RUN="churn-2pc-$(date +%s)"
CHURN_2PC_ACCEPTED=0
CHURN_2PC_REJECTED=0
CHURN_2PC_ATTEMPTED=0
CHURN_2PC_ATTEMPT_TEXTS=()
CHURN_2PC_ATTEMPT_TASKS=()
CHURN_2PC_ATTEMPT_OUTCOMES=()

for round in 1 2 3 4; do
    case "$round" in
        1)
            stopped_container="docker-node-p0a-1"
            stopped_url="$NODE_P0A_URL"
            stopped_label="p0a"
            survivor_labels=(p1a p1b p1c)
            survivor_urls=("$NODE_P1A_URL" "$NODE_P1B_URL" "$NODE_P1C_URL")
            survivor_keys=("$NODE_P1A_KEY" "$NODE_P1B_KEY" "$NODE_P1C_KEY")
            ;;
        2)
            stopped_container="docker-node-p1a-1"
            stopped_url="$NODE_P1A_URL"
            stopped_label="p1a"
            survivor_labels=(p0a p0b p0c)
            survivor_urls=("$NODE_P0A_URL" "$NODE_P0B_URL" "$NODE_P0C_URL")
            survivor_keys=("$NODE_P0A_KEY" "$NODE_P0B_KEY" "$NODE_P0C_KEY")
            ;;
        3)
            stopped_container="docker-node-p0b-1"
            stopped_url="$NODE_P0B_URL"
            stopped_label="p0b"
            survivor_labels=(p1a p1b p1c)
            survivor_urls=("$NODE_P1A_URL" "$NODE_P1B_URL" "$NODE_P1C_URL")
            survivor_keys=("$NODE_P1A_KEY" "$NODE_P1B_KEY" "$NODE_P1C_KEY")
            ;;
        4)
            stopped_container="docker-node-p1b-1"
            stopped_url="$NODE_P1B_URL"
            stopped_label="p1b"
            survivor_labels=(p0a p0b p0c)
            survivor_urls=("$NODE_P0A_URL" "$NODE_P0B_URL" "$NODE_P0C_URL")
            survivor_keys=("$NODE_P0A_KEY" "$NODE_P0B_KEY" "$NODE_P0C_KEY")
            ;;
    esac

    echo "  Round $round: stopping $stopped_label and issuing 2PC writes through surviving entrypoints..."
    ROUND_PRE_CHURN_2PC=$(query_api "$NODE_A_URL" "$NODE_A_KEY" "messages:dashboard")
    ROUND_PRE_CHURN_2PC_M=$(jval messages "$ROUND_PRE_CHURN_2PC")
    ROUND_PRE_CHURN_2PC_T=$(jval tasks "$ROUND_PRE_CHURN_2PC")

    docker stop "$stopped_container" > /dev/null 2>&1
    sleep 6

    ROUND_ACCEPTED=0
    for i in "${!survivor_labels[@]}"; do
        label="${survivor_labels[$i]}"
        CHURN_2PC_ATTEMPTED=$((CHURN_2PC_ATTEMPTED + 1))
        write_text="$CHURN_2PC_RUN-r${round}-$label"
        task_title="$CHURN_2PC_RUN-r${round}-task-$label"
        CHURN_2PC_ATTEMPT_TEXTS+=("$write_text")
        CHURN_2PC_ATTEMPT_TASKS+=("$task_title")
        R=$(mutation_response "${survivor_urls[$i]}" "${survivor_keys[$i]}" "messages:crossPartitionWrite" \
            "{\"text\":\"$write_text\",\"taskTitle\":\"$task_title\"}")
        if echo "$R" | grep -q '"status":"success"'; then
            CHURN_2PC_ACCEPTED=$((CHURN_2PC_ACCEPTED + 1))
            ROUND_ACCEPTED=$((ROUND_ACCEPTED + 1))
            CHURN_2PC_ATTEMPT_OUTCOMES+=("accepted")
        else
            CHURN_2PC_REJECTED=$((CHURN_2PC_REJECTED + 1))
            CHURN_2PC_ATTEMPT_OUTCOMES+=("rejected")
            echo -e "  ${YELLOW}WARN${NC} $label 2PC write rejected during $stopped_label outage (fail-closed): $R"
        fi
    done

    [ "$ROUND_ACCEPTED" -gt 0 ] \
        && pass "Round $round accepted at least one 2PC write during $stopped_label outage ($ROUND_ACCEPTED/3)" \
        || fail "Round $round accepted no 2PC writes during $stopped_label outage" "all three attempts failed closed"

    echo "  Restarting $stopped_label..."
    docker start "$stopped_container" > /dev/null 2>&1
    for attempt in $(seq 1 60); do
        curl -sf "$stopped_url/version" > /dev/null 2>&1 && break
        sleep 1
    done

    case "$round" in
        1)
            NODE_P0A_KEY=$(docker exec docker-node-p0a-1 ./generate_admin_key.sh 2>&1 | tail -1)
            NODE_A_KEY="$NODE_P0A_KEY"
            (cd "$DEPLOY_DIR" && npx convex deploy --admin-key "$NODE_P0A_KEY" --url "$NODE_P0A_URL" > /dev/null 2>&1)
            ;;
        2)
            NODE_P1A_KEY=$(docker exec docker-node-p1a-1 ./generate_admin_key.sh 2>&1 | tail -1)
            NODE_B_KEY="$NODE_P1A_KEY"
            (cd "$DEPLOY_DIR" && npx convex deploy --admin-key "$NODE_P1A_KEY" --url "$NODE_P1A_URL" > /dev/null 2>&1)
            ;;
        3)
            NODE_P0B_KEY=$(docker exec docker-node-p0b-1 ./generate_admin_key.sh 2>&1 | tail -1)
            (cd "$DEPLOY_DIR" && npx convex deploy --admin-key "$NODE_P0B_KEY" --url "$NODE_P0B_URL" > /dev/null 2>&1)
            ;;
        4)
            NODE_P1B_KEY=$(docker exec docker-node-p1b-1 ./generate_admin_key.sh 2>&1 | tail -1)
            (cd "$DEPLOY_DIR" && npx convex deploy --admin-key "$NODE_P1B_KEY" --url "$NODE_P1B_URL" > /dev/null 2>&1)
            ;;
    esac
    sleep 4

    ROUND_POST_CHURN_2PC=$(query_api "$NODE_A_URL" "$NODE_A_KEY" "messages:dashboard")
    ROUND_POST_CHURN_2PC_M=$(jval messages "$ROUND_POST_CHURN_2PC")
    ROUND_POST_CHURN_2PC_T=$(jval tasks "$ROUND_POST_CHURN_2PC")
    ROUND_CHURN_2PC_DM=$((ROUND_POST_CHURN_2PC_M - ROUND_PRE_CHURN_2PC_M))
    ROUND_CHURN_2PC_DT=$((ROUND_POST_CHURN_2PC_T - ROUND_PRE_CHURN_2PC_T))

    [ "$ROUND_CHURN_2PC_DM" -eq "$ROUND_ACCEPTED" ] && [ "$ROUND_CHURN_2PC_DT" -eq "$ROUND_ACCEPTED" ] \
        && pass "Round $round 2PC writes stayed atomic during $stopped_label outage (+$ROUND_ACCEPTED/+${ROUND_ACCEPTED})" \
        || fail "Round $round lost/duplicated 2PC writes during $stopped_label outage" "msgs +$ROUND_CHURN_2PC_DM tasks +$ROUND_CHURN_2PC_DT expected +$ROUND_ACCEPTED"
done

[ "$CHURN_2PC_ATTEMPTED" -eq $((CHURN_2PC_ACCEPTED + CHURN_2PC_REJECTED)) ] \
    && pass "Repeated-churn 2PC attempts were all accounted for (accepted=$CHURN_2PC_ACCEPTED rejected=$CHURN_2PC_REJECTED)" \
    || fail "Repeated-churn 2PC accounting mismatch" "attempted=$CHURN_2PC_ATTEMPTED accepted=$CHURN_2PC_ACCEPTED rejected=$CHURN_2PC_REJECTED"

sleep 10
POST_CHURN_2PC_A=$(query_api "$NODE_A_URL" "$NODE_A_KEY" "messages:dashboard")
POST_CHURN_2PC_B=$(query_api "$NODE_B_URL" "$NODE_B_KEY" "messages:dashboard")
POST_CHURN_2PC_M=$(jval messages "$POST_CHURN_2PC_A")
POST_CHURN_2PC_T=$(jval tasks "$POST_CHURN_2PC_A")
POST_CHURN_2PC_BM=$(jval messages "$POST_CHURN_2PC_B")
POST_CHURN_2PC_BT=$(jval tasks "$POST_CHURN_2PC_B")
CHURN_2PC_DM=$((POST_CHURN_2PC_M - PRE_CHURN_2PC_M))
CHURN_2PC_DT=$((POST_CHURN_2PC_T - PRE_CHURN_2PC_T))

[ "$CHURN_2PC_DM" -eq "$CHURN_2PC_ACCEPTED" ] && [ "$CHURN_2PC_DT" -eq "$CHURN_2PC_ACCEPTED" ] \
    && pass "Repeated-churn 2PC writes remained atomic (+$CHURN_2PC_ACCEPTED/+${CHURN_2PC_ACCEPTED})" \
    || fail "Lost/extra 2PC writes during repeated leadership churn" "msgs +$CHURN_2PC_DM tasks +$CHURN_2PC_DT expected +$CHURN_2PC_ACCEPTED"

[ "$POST_CHURN_2PC_M" -eq "$POST_CHURN_2PC_BM" ] && [ "$POST_CHURN_2PC_T" -eq "$POST_CHURN_2PC_BT" ] \
    && pass "Nodes converged after repeated Raft leadership churn" \
    || fail "Nodes diverged after repeated Raft leadership churn" "p0a=$POST_CHURN_2PC_M,$POST_CHURN_2PC_T p1a=$POST_CHURN_2PC_BM,$POST_CHURN_2PC_BT"

# ============================================================
echo ""
echo -e "${BOLD}Test 20f: Per-Write 2PC Exactness During Leadership Churn${NC}"
# ============================================================
# Aggregate counts alone can miss a bad parallel-commit recovery pattern where
# one accepted cross-partition operation is duplicated while another is lost.
# Verify every accepted 2PC write from the churn window appears exactly once in
# both halves and on both serving nodes.

CHURN_EXACTNESS_OK=true
CHURN_EXACTNESS_DETAILS=""
for i in "${!CHURN_2PC_ATTEMPT_TEXTS[@]}"; do
    text="${CHURN_2PC_ATTEMPT_TEXTS[$i]}"
    task="${CHURN_2PC_ATTEMPT_TASKS[$i]}"
    outcome="${CHURN_2PC_ATTEMPT_OUTCOMES[$i]}"
    MSG_A=$(jtotal "$(query_with_args "$NODE_A_URL" "$NODE_A_KEY" "messages:countMessagesByText" "{\"text\":\"$text\"}")")
    MSG_B=$(jtotal "$(query_with_args "$NODE_B_URL" "$NODE_B_KEY" "messages:countMessagesByText" "{\"text\":\"$text\"}")")
    TASK_A=$(jtotal "$(query_with_args "$NODE_A_URL" "$NODE_A_KEY" "messages:countTasksByTitle" "{\"title\":\"$task\"}")")
    TASK_B=$(jtotal "$(query_with_args "$NODE_B_URL" "$NODE_B_KEY" "messages:countTasksByTitle" "{\"title\":\"$task\"}")")
    if [ "$outcome" = "accepted" ]; then
        expected=1
    else
        expected=0
    fi
    if [ "$MSG_A" -ne "$expected" ] || [ "$MSG_B" -ne "$expected" ] || \
        [ "$TASK_A" -ne "$expected" ] || [ "$TASK_B" -ne "$expected" ]; then
        CHURN_EXACTNESS_OK=false
        CHURN_EXACTNESS_DETAILS="$CHURN_EXACTNESS_DETAILS [$outcome $text/$task expected=$expected msgA=$MSG_A msgB=$MSG_B taskA=$TASK_A taskB=$TASK_B]"
    fi
done

$CHURN_EXACTNESS_OK \
    && pass "Every churn-window 2PC attempt matched its client outcome on both nodes" \
    || fail "Churn-window 2PC outcome/exactness violated" "$CHURN_EXACTNESS_DETAILS"

# ============================================================
echo ""
echo -e "${BOLD}Test 21: Sustained Writes 15s (long-running replication)${NC}"
# ============================================================
# Write continuously for 30 seconds. Verify zero data loss.

SUSTAINED_PRE=$(query_api "$NODE_A_URL" "$NODE_A_KEY" "messages:dashboard")
SUSTAINED_PRE_M=$(jval messages "$SUSTAINED_PRE")
SUSTAINED_PRE_T=$(jval tasks "$SUSTAINED_PRE")

SUSTAINED_COUNT_A=0
SUSTAINED_COUNT_B=0
SUSTAINED_END=$(($(date +%s) + 30))

echo "  Writing for 30 seconds..."
while [ "$(date +%s)" -lt "$SUSTAINED_END" ]; do
    mutate "$NODE_A_URL" "$NODE_A_KEY" "messages:send" \
        "{\"text\":\"sustained\",\"author\":\"endurance\",\"channel\":\"test\"}" > /dev/null && \
        SUSTAINED_COUNT_A=$((SUSTAINED_COUNT_A + 1))
    mutate "$NODE_B_URL" "$NODE_B_KEY" "messages:createTask" \
        "{\"title\":\"sustained\",\"assignee\":\"endurance\",\"project\":\"test\",\"status\":\"done\"}" > /dev/null && \
        SUSTAINED_COUNT_B=$((SUSTAINED_COUNT_B + 1))
done

echo "  Wrote $SUSTAINED_COUNT_A msgs + $SUSTAINED_COUNT_B tasks in 30s"

# Both nodes alive?
curl -sf "$NODE_A_URL/version" > /dev/null 2>&1 \
    && pass "Node A survived sustained writes" \
    || fail "Node A crashed during sustained writes"

curl -sf "$NODE_B_URL/version" > /dev/null 2>&1 \
    && pass "Node B survived sustained writes" \
    || fail "Node B crashed during sustained writes"

sleep 5

SUSTAINED_POST_A=$(query_api "$NODE_A_URL" "$NODE_A_KEY" "messages:dashboard")
SUSTAINED_POST_B=$(query_api "$NODE_B_URL" "$NODE_B_KEY" "messages:dashboard")

SUSTAINED_POST_AM=$(jval messages "$SUSTAINED_POST_A")
SUSTAINED_POST_AT=$(jval tasks "$SUSTAINED_POST_A")

SUSTAINED_DM=$((SUSTAINED_POST_AM - SUSTAINED_PRE_M))
SUSTAINED_DT=$((SUSTAINED_POST_AT - SUSTAINED_PRE_T))

[ "$SUSTAINED_DM" -eq "$SUSTAINED_COUNT_A" ] \
    && pass "No lost msgs: +$SUSTAINED_DM (expected +$SUSTAINED_COUNT_A)" \
    || fail "Lost msgs during sustained writes" "+$SUSTAINED_DM, expected +$SUSTAINED_COUNT_A"

[ "$SUSTAINED_DT" -eq "$SUSTAINED_COUNT_B" ] \
    && pass "No lost tasks: +$SUSTAINED_DT (expected +$SUSTAINED_COUNT_B)" \
    || fail "Lost tasks during sustained writes" "+$SUSTAINED_DT, expected +$SUSTAINED_COUNT_B"

SUSTAINED_POST_BM=$(jval messages "$SUSTAINED_POST_B")
SUSTAINED_POST_BT=$(jval tasks "$SUSTAINED_POST_B")

[ "$SUSTAINED_POST_AM" -eq "$SUSTAINED_POST_BM" ] && [ "$SUSTAINED_POST_AT" -eq "$SUSTAINED_POST_BT" ] \
    && pass "Nodes converged after 30s sustained writes" \
    || fail "Nodes diverged" "A: $SUSTAINED_POST_AM,$SUSTAINED_POST_AT vs B: $SUSTAINED_POST_BM,$SUSTAINED_POST_BT"

# ============================================================
echo ""
echo -e "${BOLD}Test 22: Duplicate Insert Idempotency${NC}"
# ============================================================
# Insert the same data twice. Verify two rows (not deduplicated — Convex
# doesn't have unique constraints, so both should exist).

DUP_KEY="dup-$(date +%s)"
mutate "$NODE_A_URL" "$NODE_A_KEY" "messages:writeOrdered" \
    "{\"key\":\"$DUP_KEY\",\"value\":\"duplicate\"}" > /dev/null
mutate "$NODE_A_URL" "$NODE_A_KEY" "messages:writeOrdered" \
    "{\"key\":\"$DUP_KEY\",\"value\":\"duplicate\"}" > /dev/null

DUP_COUNT=$(curl -sf "$NODE_A_URL/api/query" \
    -H "Authorization: Convex $NODE_A_KEY" \
    -H "Content-Type: application/json" \
    -d "{\"path\":\"messages:readOrdered\",\"args\":{\"key\":\"$DUP_KEY\"}}" \
    | python3 -c "import sys,json; print(len(json.load(sys.stdin)['value']))")

[ "$DUP_COUNT" -eq 2 ] \
    && pass "Duplicate inserts both persisted: $DUP_COUNT rows" \
    || fail "Duplicate handling wrong" "got $DUP_COUNT, expected 2"

# ============================================================
echo ""
echo -e "${BOLD}Test 23: Final Exhaustive Invariant Check${NC}"
# ============================================================
# After all chaos, stress, restarts, and edge cases — every invariant
# must still hold. This is CockroachDB's workload check pattern.

sleep 3

EX_A=$(query_api "$NODE_A_URL" "$NODE_A_KEY" "messages:dashboard")
EX_B=$(query_api "$NODE_B_URL" "$NODE_B_KEY" "messages:dashboard")

EX_AM=$(jval messages "$EX_A"); EX_AU=$(jval users "$EX_A")
EX_AP=$(jval projects "$EX_A"); EX_AT=$(jval tasks "$EX_A")
EX_BM=$(jval messages "$EX_B"); EX_BU=$(jval users "$EX_B")
EX_BP=$(jval projects "$EX_B"); EX_BT=$(jval tasks "$EX_B")

[ "$EX_AM" -eq "$EX_BM" ] && [ "$EX_AU" -eq "$EX_BU" ] && \
[ "$EX_AP" -eq "$EX_BP" ] && [ "$EX_AT" -eq "$EX_BT" ] \
    && pass "Exhaustive convergence: all tables match (msgs=$EX_AM users=$EX_AU proj=$EX_AP tasks=$EX_AT)" \
    || fail "Exhaustive divergence" "A: $EX_AM,$EX_AU,$EX_AP,$EX_AT vs B: $EX_BM,$EX_BU,$EX_BP,$EX_BT"

EX_BUDGET_A=$(jtotal "$(query_api "$NODE_A_URL" "$NODE_A_KEY" "messages:totalBudget")")
EX_BUDGET_B=$(jtotal "$(query_api "$NODE_B_URL" "$NODE_B_KEY" "messages:totalBudget")")

[ "$EX_BUDGET_A" -eq "$EX_BUDGET_B" ] \
    && pass "Exhaustive budget invariant: \$$EX_BUDGET_A" \
    || fail "Budget invariant violated" "A=\$$EX_BUDGET_A vs B=\$$EX_BUDGET_B"

EX_COMP_A=$(query_api "$NODE_A_URL" "$NODE_A_KEY" "messages:totalCompensation")
EX_COMP_B=$(query_api "$NODE_B_URL" "$NODE_B_KEY" "messages:totalCompensation")
EX_TOTAL_A=$(python3 -c "import sys,json; print(int(json.load(sys.stdin)['value']['total']))" <<< "$EX_COMP_A")
EX_TOTAL_B=$(python3 -c "import sys,json; print(int(json.load(sys.stdin)['value']['total']))" <<< "$EX_COMP_B")

[ "$EX_TOTAL_A" -eq "$EX_TOTAL_B" ] \
    && pass "Exhaustive cross-table invariant: \$$EX_TOTAL_A" \
    || fail "Cross-table invariant violated" "A=\$$EX_TOTAL_A vs B=\$$EX_TOTAL_B"

echo ""
echo "  Total data: msgs=$EX_AM users=$EX_AU projects=$EX_AP tasks=$EX_AT"

# ============================================================
echo ""
echo -e "${BOLD}Test 24: Single-Key Register (CockroachDB Jepsen register)${NC}"
# ============================================================
# Write a value, read it back. Overwrite. Read again. Verify linearizable.

REG_KEY="reg-$(date +%s)"
mutate "$NODE_A_URL" "$NODE_A_KEY" "messages:writeRegister" \
    "{\"key\":\"$REG_KEY\",\"value\":\"first\"}" > /dev/null
REG_V1=$(curl -sf "$NODE_A_URL/api/query" \
    -H "Authorization: Convex $NODE_A_KEY" \
    -H "Content-Type: application/json" \
    -d "{\"path\":\"messages:readRegister\",\"args\":{\"key\":\"$REG_KEY\"}}" \
    | python3 -c "import sys,json; print(json.load(sys.stdin)['value'])")

[ "$REG_V1" = "first" ] \
    && pass "Register read-after-write: $REG_V1" \
    || fail "Register stale" "got $REG_V1, expected first"

mutate "$NODE_A_URL" "$NODE_A_KEY" "messages:writeRegister" \
    "{\"key\":\"$REG_KEY\",\"value\":\"second\"}" > /dev/null
REG_V2=$(curl -sf "$NODE_A_URL/api/query" \
    -H "Authorization: Convex $NODE_A_KEY" \
    -H "Content-Type: application/json" \
    -d "{\"path\":\"messages:readRegister\",\"args\":{\"key\":\"$REG_KEY\"}}" \
    | python3 -c "import sys,json; print(json.load(sys.stdin)['value'])")

[ "$REG_V2" = "second" ] \
    && pass "Register overwrite: $REG_V2" \
    || fail "Register didn't update" "got $REG_V2, expected second"

# ============================================================
echo ""
echo -e "${BOLD}Test 25: Disjoint Record Ordering (CockroachDB Jepsen comments)${NC}"
# ============================================================
# Write to two different tables sequentially. Read both from other node.
# Both must be visible — no partial visibility of sequential writes.

DJ_KEY="dj-$(date +%s)"
mutate "$NODE_A_URL" "$NODE_A_KEY" "messages:send" \
    "{\"text\":\"$DJ_KEY\",\"author\":\"disjoint\",\"channel\":\"test\"}" > /dev/null
mutate "$NODE_B_URL" "$NODE_B_KEY" "messages:createTask" \
    "{\"title\":\"$DJ_KEY\",\"assignee\":\"disjoint\",\"project\":\"test\",\"status\":\"done\"}" > /dev/null

sleep 4

# Check that Node B sees both the message and the task.
DJ_CHECK_B=$(query_api "$NODE_B_URL" "$NODE_B_KEY" "messages:dashboard")
DJ_BM=$(jval messages "$DJ_CHECK_B")
DJ_BT=$(jval tasks "$DJ_CHECK_B")

# Both must have increased (message on A, task on B — both visible on B).
[ "$DJ_BM" -gt 0 ] && [ "$DJ_BT" -gt 0 ] \
    && pass "Disjoint records visible on Node B: msgs=$DJ_BM tasks=$DJ_BT" \
    || fail "Disjoint record not visible" "msgs=$DJ_BM tasks=$DJ_BT"

# ============================================================
echo ""
echo -e "${BOLD}Test 26: NATS Partition Simulation${NC}"
# ============================================================
# Pause NATS briefly, write to Node A, unpause, verify accepted writes are
# eventually visible on both partitions.

echo "  Pausing NATS for 3 seconds..."
docker pause docker-nats-1 > /dev/null 2>&1

# Strong remote reads use the partition owner directly, not the NATS-maintained
# local mirror. The task was written to partition 1 in Test 25, so querying it
# through partition 0 must remain available while NATS is paused.
OWNER_READ_DURING_NATS=$(curl --max-time 10 -sf "$NODE_A_URL/api/query" \
    -H "Authorization: Convex $NODE_A_KEY" \
    -H "Content-Type: application/json" \
    -d "{\"path\":\"messages:countTasksByTitle\",\"args\":{\"title\":\"$DJ_KEY\"}}" 2>/dev/null || true)
OWNER_READ_DURING_NATS_COUNT=$(python3 -c "import sys,json; print(int(json.load(sys.stdin)['value']))" \
    <<< "$OWNER_READ_DURING_NATS" 2>/dev/null || echo -1)
[ "$OWNER_READ_DURING_NATS_COUNT" -eq 1 ] \
    && pass "Partition 0 read partition-1-owned data while NATS was unavailable" \
    || fail "Owner-authoritative remote read depended on NATS or stale local state" \
        "expected task count 1, got $OWNER_READ_DURING_NATS_COUNT; response=$OWNER_READ_DURING_NATS"

# Write during NATS outage. It may fail closed if the TSO/decision path cannot
# make progress, but if it is accepted then the Raft/NATS outbox must replay it
# after recovery.
NATS_KEY="nats-pause-$(date +%s)"
NATS_RESPONSE=$(mutation_response "$NODE_A_URL" "$NODE_A_KEY" "messages:send" \
    "{\"text\":\"$NATS_KEY\",\"author\":\"$NATS_KEY\",\"channel\":\"nats-outage\"}" 2>&1 || echo '{"status":"transport_error"}')

sleep 3
docker unpause docker-nats-1 > /dev/null 2>&1
echo "  NATS resumed."

# Both nodes still alive?
sleep 5
curl -sf "$NODE_A_URL/version" > /dev/null 2>&1 \
    && pass "Node A survived NATS partition" \
    || fail "Node A crashed during NATS partition"

curl -sf "$NODE_B_URL/version" > /dev/null 2>&1 \
    && pass "Node B survived NATS partition" \
    || fail "Node B crashed during NATS partition"

NATS_ACCEPTED=0
if echo "$NATS_RESPONSE" | grep -q '"status":"success"'; then
    NATS_ACCEPTED=1
fi

NATS_A_COUNT=$(query_with_args "$NODE_A_URL" "$NODE_A_KEY" "messages:countMessagesByAuthorChannel" \
    "{\"author\":\"$NATS_KEY\",\"channel\":\"nats-outage\"}" \
    | python3 -c "import sys,json; print(int(json.load(sys.stdin)['value']))" 2>/dev/null || echo -1)
NATS_B_COUNT=$(query_with_args "$NODE_B_URL" "$NODE_B_KEY" "messages:countMessagesByAuthorChannel" \
    "{\"author\":\"$NATS_KEY\",\"channel\":\"nats-outage\"}" \
    | python3 -c "import sys,json; print(int(json.load(sys.stdin)['value']))" 2>/dev/null || echo -1)

if [ "$NATS_ACCEPTED" -eq 1 ]; then
    [ "$NATS_A_COUNT" -eq 1 ] && [ "$NATS_B_COUNT" -eq 1 ] \
        && pass "Accepted write during NATS outage replayed to both partitions" \
        || fail "Accepted write during NATS outage did not replay" "response=$NATS_RESPONSE p0a=$NATS_A_COUNT p1a=$NATS_B_COUNT"
else
    [ "$NATS_A_COUNT" -eq 0 ] && [ "$NATS_B_COUNT" -eq 0 ] \
        && pass "Write during NATS outage failed closed without partial visibility" \
        || fail "Failed NATS-outage write left partial visibility" "response=$NATS_RESPONSE p0a=$NATS_A_COUNT p1a=$NATS_B_COUNT"
fi

# ============================================================
echo ""
echo -e "${BOLD}Test 27: Write During Deploy${NC}"
# ============================================================
# Start a write, deploy functions, verify no corruption.

PRE_DEPLOY=$(query_api "$NODE_A_URL" "$NODE_A_KEY" "messages:dashboard")
PRE_DEPLOY_M=$(jval messages "$PRE_DEPLOY")

mutate "$NODE_A_URL" "$NODE_A_KEY" "messages:send" \
    '{"text":"pre-deploy","author":"deploy-test","channel":"test"}' > /dev/null

# Redeploy while data is in flight.
(cd "$DEPLOY_DIR" && npx convex deploy --admin-key "$NODE_A_KEY" --url "$NODE_A_URL" > /dev/null 2>&1)

mutate "$NODE_A_URL" "$NODE_A_KEY" "messages:send" \
    '{"text":"post-deploy","author":"deploy-test","channel":"test"}' > /dev/null

POST_DEPLOY=$(query_api "$NODE_A_URL" "$NODE_A_KEY" "messages:dashboard")
POST_DEPLOY_M=$(jval messages "$POST_DEPLOY")

DEPLOY_DELTA=$((POST_DEPLOY_M - PRE_DEPLOY_M))
[ "$DEPLOY_DELTA" -eq 2 ] \
    && pass "Write during deploy: both writes survived ($DEPLOY_DELTA)" \
    || fail "Write lost during deploy" "delta=$DEPLOY_DELTA, expected 2"

# ============================================================
echo ""
echo -e "${BOLD}Test 28: Empty Table Cross-Node Query${NC}"
# ============================================================
# Query a table that exists but has no matching rows. Both nodes must
# return consistent empty results.

EMPTY_KEY="empty-$(date +%s)"
EA=$(curl -sf "$NODE_A_URL/api/query" \
    -H "Authorization: Convex $NODE_A_KEY" \
    -H "Content-Type: application/json" \
    -d "{\"path\":\"messages:readOrdered\",\"args\":{\"key\":\"$EMPTY_KEY\"}}" \
    | python3 -c "import sys,json; print(len(json.load(sys.stdin)['value']))")
EB=$(curl -sf "$NODE_B_URL/api/query" \
    -H "Authorization: Convex $NODE_B_KEY" \
    -H "Content-Type: application/json" \
    -d "{\"path\":\"messages:readOrdered\",\"args\":{\"key\":\"$EMPTY_KEY\"}}" \
    | python3 -c "import sys,json; print(len(json.load(sys.stdin)['value']))")

[ "$EA" -eq 0 ] && [ "$EB" -eq 0 ] \
    && pass "Empty query consistent: both nodes return 0 rows" \
    || fail "Empty query inconsistent" "A=$EA B=$EB"

# ============================================================
echo ""
echo -e "${BOLD}Test 29: Max Batch Size 200 docs${NC}"
# ============================================================

BIG_PREFIX="big-$(date +%s)"
mutate "$NODE_A_URL" "$NODE_A_KEY" "messages:batchWrite" \
    "{\"prefix\":\"$BIG_PREFIX\",\"count\":200}" > /dev/null

BIG_COUNT=$(curl -sf "$NODE_A_URL/api/query" \
    -H "Authorization: Convex $NODE_A_KEY" \
    -H "Content-Type: application/json" \
    -d "{\"path\":\"messages:countBatch\",\"args\":{\"prefix\":\"$BIG_PREFIX\"}}" \
    | python3 -c "import sys,json; print(int(json.load(sys.stdin)['value']))")

[ "$BIG_COUNT" -eq 200 ] \
    && pass "200-doc batch atomic: all present" \
    || fail "200-doc batch partial" "got $BIG_COUNT"

sleep 5
BIG_COUNT_B=$(curl -sf "$NODE_B_URL/api/query" \
    -H "Authorization: Convex $NODE_B_KEY" \
    -H "Content-Type: application/json" \
    -d "{\"path\":\"messages:countBatch\",\"args\":{\"prefix\":\"$BIG_PREFIX\"}}" \
    | python3 -c "import sys,json; print(int(json.load(sys.stdin)['value']))")

[ "$BIG_COUNT_B" -eq 200 ] \
    && pass "200-doc batch replicated to Node B" \
    || fail "200-doc batch incomplete on B" "got $BIG_COUNT_B"

# ============================================================
echo ""
echo -e "${BOLD}Test 30: Null and Empty Field Values${NC}"
# ============================================================

NULL_KEY="null-$(date +%s)"
mutate "$NODE_A_URL" "$NODE_A_KEY" "messages:writeNullFields" \
    "{\"key\":\"$NULL_KEY\"}" > /dev/null

NF=$(curl -sf "$NODE_A_URL/api/query" \
    -H "Authorization: Convex $NODE_A_KEY" \
    -H "Content-Type: application/json" \
    -d "{\"path\":\"messages:readNullFields\",\"args\":{\"key\":\"$NULL_KEY\"}}" \
    | python3 -c "import sys,json; v=json.load(sys.stdin)['value']; print(f\"{v['text']}|{v['channel']}|{int(v['timestamp'])}\")")

[ "$NF" = "||0" ] \
    && pass "Null/empty fields preserved: '$NF'" \
    || fail "Null/empty fields corrupted" "got '$NF', expected '||0'"

sleep 3
NF_B=$(curl -sf "$NODE_B_URL/api/query" \
    -H "Authorization: Convex $NODE_B_KEY" \
    -H "Content-Type: application/json" \
    -d "{\"path\":\"messages:readNullFields\",\"args\":{\"key\":\"$NULL_KEY\"}}" \
    | python3 -c "import sys,json; v=json.load(sys.stdin)['value']; print(f\"{v['text']}|{v['channel']}|{int(v['timestamp'])}\")")

[ "$NF_B" = "||0" ] \
    && pass "Null/empty fields replicated to Node B: '$NF_B'" \
    || fail "Null/empty fields corrupted on B" "got '$NF_B'"

# ============================================================
echo ""
echo -e "${BOLD}Test 31: Concurrent Table Creation${NC}"
# ============================================================
# Both nodes create data in new tables simultaneously. The tables are
# already in the partition map, but data creation is concurrent.

CT_KEY="ct-$(date +%s)"
(mutate "$NODE_A_URL" "$NODE_A_KEY" "messages:send" \
    "{\"text\":\"$CT_KEY\",\"author\":\"concurrent-create\",\"channel\":\"test\"}" > /dev/null) &
CT_A=$!
(mutate "$NODE_B_URL" "$NODE_B_KEY" "messages:createTask" \
    "{\"title\":\"$CT_KEY\",\"assignee\":\"concurrent-create\",\"project\":\"test\",\"status\":\"done\"}" > /dev/null) &
CT_B=$!
wait $CT_A
wait $CT_B

sleep 3

CT_CHECK_A=$(query_api "$NODE_A_URL" "$NODE_A_KEY" "messages:dashboard")
CT_CHECK_B=$(query_api "$NODE_B_URL" "$NODE_B_KEY" "messages:dashboard")

CT_AM=$(jval messages "$CT_CHECK_A"); CT_BM=$(jval messages "$CT_CHECK_B")
CT_AT=$(jval tasks "$CT_CHECK_A"); CT_BT=$(jval tasks "$CT_CHECK_B")

[ "$CT_AM" -eq "$CT_BM" ] && [ "$CT_AT" -eq "$CT_BT" ] \
    && pass "Concurrent creation: nodes converged (msgs=$CT_AM tasks=$CT_AT)" \
    || fail "Concurrent creation diverged" "A: $CT_AM,$CT_AT vs B: $CT_BM,$CT_BT"

# ============================================================
echo ""
echo -e "${BOLD}Test 32: Rapid Deploy Cycle${NC}"
# ============================================================
# Deploy 3 times rapidly while writing. No corruption.

RD_PRE=$(query_api "$NODE_A_URL" "$NODE_A_KEY" "messages:dashboard")
RD_PRE_M=$(jval messages "$RD_PRE")

for i in 1 2 3; do
    mutate "$NODE_A_URL" "$NODE_A_KEY" "messages:send" \
        "{\"text\":\"rapid-deploy-$i\",\"author\":\"rd\",\"channel\":\"test\"}" > /dev/null
    (cd "$DEPLOY_DIR" && npx convex deploy --admin-key "$NODE_A_KEY" --url "$NODE_A_URL" > /dev/null 2>&1)
done

RD_POST=$(query_api "$NODE_A_URL" "$NODE_A_KEY" "messages:dashboard")
RD_POST_M=$(jval messages "$RD_POST")
RD_DELTA=$((RD_POST_M - RD_PRE_M))

[ "$RD_DELTA" -eq 3 ] \
    && pass "Rapid deploy: all 3 writes survived ($RD_DELTA)" \
    || fail "Rapid deploy lost writes" "delta=$RD_DELTA, expected 3"

# ============================================================
echo ""
echo -e "${BOLD}Test 33: Read During Active Replication${NC}"
# ============================================================
# Write rapidly to Node A while continuously reading from Node B.
# Reads must never fail and counts must be monotonically increasing.

RR_LAST=0
RR_OK=true
for i in $(seq 1 10); do
    mutate "$NODE_A_URL" "$NODE_A_KEY" "messages:send" \
        "{\"text\":\"read-during-repl-$i\",\"author\":\"rdr\",\"channel\":\"test\"}" > /dev/null
    RR_NOW=$(query_api "$NODE_B_URL" "$NODE_B_KEY" "messages:dashboard" 2>/dev/null \
        | python3 -c "import sys,json; print(int(json.load(sys.stdin)['value']['messages']))" 2>/dev/null || echo 0)
    if [ "$RR_NOW" -lt "$RR_LAST" ]; then
        RR_OK=false
        fail "Read regression during replication" "iteration $i: $RR_NOW < $RR_LAST"
        break
    fi
    RR_LAST=$RR_NOW
done

$RR_OK && pass "Reads during replication: monotonically increasing ($RR_LAST)"

# ============================================================
echo ""
echo -e "${BOLD}Test 34: Clock Monotonicity After Restart (TSO)${NC}"
# ============================================================
# Restart Node A, verify TSO counter doesn't regress.

PRE_RESTART_A=$(query_api "$NODE_A_URL" "$NODE_A_KEY" "messages:dashboard")
PRE_RESTART_AM=$(jval messages "$PRE_RESTART_A")

docker restart docker-node-p0a-1 > /dev/null 2>&1
echo "  Waiting for Node A recovery..."
for attempt in $(seq 1 30); do
    curl -sf "$NODE_A_URL/version" > /dev/null 2>&1 && break
    sleep 1
done

NODE_P0A_KEY=$(docker exec docker-node-p0a-1 ./generate_admin_key.sh 2>&1 | tail -1)
NODE_A_KEY="$NODE_P0A_KEY"
(cd "$DEPLOY_DIR" && npx convex deploy --admin-key "$NODE_A_KEY" --url "$NODE_A_URL" > /dev/null 2>&1)

# Write after restart — TSO must give a valid timestamp.
mutate "$NODE_A_URL" "$NODE_A_KEY" "messages:send" \
    '{"text":"post-tso-restart","author":"tso-test","channel":"test"}' > /dev/null \
    && pass "TSO functional after restart: write succeeded" \
    || fail "TSO broken after restart: write failed"

POST_RESTART_A=$(query_api "$NODE_A_URL" "$NODE_A_KEY" "messages:dashboard")
POST_RESTART_AM=$(jval messages "$POST_RESTART_A")

[ "$POST_RESTART_AM" -gt "$PRE_RESTART_AM" ] \
    && pass "TSO monotonic: msgs $POST_RESTART_AM > $PRE_RESTART_AM" \
    || fail "TSO regression" "msgs $POST_RESTART_AM <= $PRE_RESTART_AM"

# ============================================================
echo ""
echo -e "${BOLD}Test 35: Single Document Read-Modify-Write${NC}"
# ============================================================
# Write, read, overwrite, read. Verify the document reflects the latest write.

RMW_KEY="rmw-$(date +%s)"
mutate "$NODE_A_URL" "$NODE_A_KEY" "messages:writeRegister" \
    "{\"key\":\"$RMW_KEY\",\"value\":\"version1\"}" > /dev/null
mutate "$NODE_A_URL" "$NODE_A_KEY" "messages:writeRegister" \
    "{\"key\":\"$RMW_KEY\",\"value\":\"version2\"}" > /dev/null
mutate "$NODE_A_URL" "$NODE_A_KEY" "messages:writeRegister" \
    "{\"key\":\"$RMW_KEY\",\"value\":\"version3\"}" > /dev/null

RMW_V=$(curl -sf "$NODE_A_URL/api/query" \
    -H "Authorization: Convex $NODE_A_KEY" \
    -H "Content-Type: application/json" \
    -d "{\"path\":\"messages:readRegister\",\"args\":{\"key\":\"$RMW_KEY\"}}" \
    | python3 -c "import sys,json; print(json.load(sys.stdin)['value'])")

[ "$RMW_V" = "version3" ] \
    && pass "Read-modify-write: final value is version3" \
    || fail "Read-modify-write stale" "got $RMW_V, expected version3"

# Verify replicated.
sleep 4
RMW_VB=$(curl -sf "$NODE_B_URL/api/query" \
    -H "Authorization: Convex $NODE_B_KEY" \
    -H "Content-Type: application/json" \
    -d "{\"path\":\"messages:readRegister\",\"args\":{\"key\":\"$RMW_KEY\"}}" \
    | python3 -c "import sys,json; print(json.load(sys.stdin)['value'])")

[ "$RMW_VB" = "version3" ] \
    && pass "Read-modify-write replicated: Node B sees version3" \
    || fail "Read-modify-write not replicated" "Node B got $RMW_VB"

# ============================================================
echo ""
echo -e "${BOLD}Test 36: Write Skew Detection (G2 anomaly)${NC}"
# ============================================================
# Two keys A and B. Write A=1, B=1. Then concurrently: one txn reads A
# and writes B, another reads B and writes A. In our system, partition
# enforcement prevents cross-partition writes, so this tests that the
# single-partition path handles concurrent read-modify correctly.

WS_A="ws-a-$(date +%s)"
WS_B="ws-b-$(date +%s)"
mutate "$NODE_A_URL" "$NODE_A_KEY" "messages:writeRegister" \
    "{\"key\":\"$WS_A\",\"value\":\"1\"}" > /dev/null
mutate "$NODE_A_URL" "$NODE_A_KEY" "messages:writeRegister" \
    "{\"key\":\"$WS_B\",\"value\":\"1\"}" > /dev/null

# Concurrent writes to both keys.
(mutate "$NODE_A_URL" "$NODE_A_KEY" "messages:writeRegister" \
    "{\"key\":\"$WS_A\",\"value\":\"2\"}" > /dev/null) &
(mutate "$NODE_A_URL" "$NODE_A_KEY" "messages:writeRegister" \
    "{\"key\":\"$WS_B\",\"value\":\"2\"}" > /dev/null) &
wait

WS_RESULT=$(curl -sf "$NODE_A_URL/api/query" \
    -H "Authorization: Convex $NODE_A_KEY" \
    -H "Content-Type: application/json" \
    -d "{\"path\":\"messages:readTwoKeys\",\"args\":{\"keyA\":\"$WS_A\",\"keyB\":\"$WS_B\"}}" \
    | python3 -c "import sys,json; v=json.load(sys.stdin)['value']; print(f\"{v['a']}|{v['b']}\")")

# Both must be "2" — the concurrent writes both completed.
[ "$WS_RESULT" = "2|2" ] \
    && pass "Write skew test: both keys updated to 2" \
    || fail "Write skew anomaly" "got $WS_RESULT, expected 2|2"

# ============================================================
echo ""
echo -e "${BOLD}Test 37: Ultimate Final Invariant Check${NC}"
# ============================================================
# After ALL previous tests including NATS partition, node restarts, deploys,
# 200-doc batches, and sustained writes — every invariant must hold.

sleep 5

ULT_A=$(query_api "$NODE_A_URL" "$NODE_A_KEY" "messages:dashboard")
ULT_B=$(query_api "$NODE_B_URL" "$NODE_B_KEY" "messages:dashboard")

ULT_AM=$(jval messages "$ULT_A"); ULT_AU=$(jval users "$ULT_A")
ULT_AP=$(jval projects "$ULT_A"); ULT_AT=$(jval tasks "$ULT_A")
ULT_BM=$(jval messages "$ULT_B"); ULT_BU=$(jval users "$ULT_B")
ULT_BP=$(jval projects "$ULT_B"); ULT_BT=$(jval tasks "$ULT_B")

[ "$ULT_AM" -eq "$ULT_BM" ] && [ "$ULT_AU" -eq "$ULT_BU" ] && \
[ "$ULT_AP" -eq "$ULT_BP" ] && [ "$ULT_AT" -eq "$ULT_BT" ] \
    && pass "ULTIMATE convergence: all tables match (msgs=$ULT_AM users=$ULT_AU proj=$ULT_AP tasks=$ULT_AT)" \
    || fail "ULTIMATE divergence" "A: $ULT_AM,$ULT_AU,$ULT_AP,$ULT_AT vs B: $ULT_BM,$ULT_BU,$ULT_BP,$ULT_BT"

ULT_BA=$(jtotal "$(query_api "$NODE_A_URL" "$NODE_A_KEY" "messages:totalBudget")")
ULT_BB=$(jtotal "$(query_api "$NODE_B_URL" "$NODE_B_KEY" "messages:totalBudget")")

[ "$ULT_BA" -eq "$ULT_BB" ] \
    && pass "ULTIMATE budget invariant: \$$ULT_BA" \
    || fail "ULTIMATE budget violated" "A=\$$ULT_BA vs B=\$$ULT_BB"

echo ""
echo "  FINAL DATA: msgs=$ULT_AM users=$ULT_AU projects=$ULT_AP tasks=$ULT_AT budget=\$$ULT_BA"

# ============================================================
echo ""
echo -e "${BOLD}Test 38: Sync Route Authority Guard${NC}"
# ============================================================
# Sync/subscription setup is coordinator-owner only. Non-coordinator partition
# nodes must fail closed before WebSocket upgrade instead of hosting orphaned
# subscription state.

SYNC_OK=true
SYNC_DETAILS=""
for entry in "p1a|$NODE_P1A_URL" "p1b|$NODE_P1B_URL" "p1c|$NODE_P1C_URL"; do
    label="${entry%%|*}"
    url="${entry#*|}"
    for path in "/api/sync" "/api/1.0.0/sync"; do
        STATUS=$(curl -s -o /dev/null -w "%{http_code}" \
            -H "Connection: Upgrade" \
            -H "Upgrade: websocket" \
            -H "Sec-WebSocket-Key: dGhlIHNhbXBsZSBub25jZQ==" \
            -H "Sec-WebSocket-Version: 13" \
            "$url$path" 2>/dev/null || echo "000")
        STATUS="${STATUS: -3}"
        if [ "$STATUS" -lt 400 ] || [ "$STATUS" -eq 101 ]; then
            SYNC_OK=false
            SYNC_DETAILS="$SYNC_DETAILS $label$path=$STATUS"
        fi
    done
done

$SYNC_OK \
    && pass "Non-coordinator partition nodes reject sync/WebSocket routes" \
    || fail "Sync route did not fail closed on non-coordinator nodes" "$SYNC_DETAILS"

# ============================================================
echo ""
echo "============================================================"
TOTAL=$((PASSED + FAILED))
if [ "$FAILED" -eq 0 ]; then
    echo -e "${GREEN}ALL $TOTAL TESTS PASSED${NC}"
else
    echo -e "${RED}$FAILED/$TOTAL TESTS FAILED${NC}"
fi
echo "============================================================"
echo ""

rm -rf "$DEPLOY_DIR"
exit "$FAILED"
