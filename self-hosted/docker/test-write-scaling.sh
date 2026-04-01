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
#   4. Partition Enforcement (Vitess Single mode)
#   5. Concurrent Write Scaling (CockroachDB KV)
#   6. Monotonic Reads (TiDB monotonic)
#   7. Node Restart Recovery (TiDB kill -9 / CockroachDB nemesis)
#   8. Idempotent Re-Run (CockroachDB workload check)
#
# Prerequisites:
#   docker compose -f docker-compose.partitioned.yml up
#
# Usage:
#   cd self-hosted/docker && ./test-write-scaling.sh

set -euo pipefail

NODE_A_URL="http://127.0.0.1:3210"
NODE_B_URL="http://127.0.0.1:3220"

RED='\033[0;31m'
GREEN='\033[0;32m'
YELLOW='\033[1;33m'
BOLD='\033[1m'
NC='\033[0m'

PASSED=0
FAILED=0

pass() {
    PASSED=$((PASSED + 1))
    echo -e "  ${GREEN}PASS${NC} $1"
}

fail() {
    FAILED=$((FAILED + 1))
    echo -e "  ${RED}FAIL${NC} $1"
    [ -n "${2:-}" ] && echo -e "       $2"
}

# --- Preflight ---

echo ""
echo -e "${BOLD}Preflight checks${NC}"

for name in docker-node-a-1 docker-node-b-1; do
    if ! docker inspect "$name" > /dev/null 2>&1; then
        echo -e "${RED}Container $name not running. Start the deployment first:${NC}"
        echo "  docker compose -f docker-compose.partitioned.yml up"
        exit 1
    fi
done
echo "  Containers running."

NODE_A_KEY=$(docker exec docker-node-a-1 ./generate_admin_key.sh 2>&1 | tail -1)
NODE_B_KEY=$(docker exec docker-node-b-1 ./generate_admin_key.sh 2>&1 | tail -1)
echo "  Admin keys generated."

# --- Deploy test functions ---

DEPLOY_DIR=$(mktemp -d)
mkdir -p "$DEPLOY_DIR/convex"

cat > "$DEPLOY_DIR/package.json" << 'EOF'
{ "name": "write-scaling-test", "version": "1.0.0", "dependencies": { "convex": "^1" } }
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
TSEOF

echo "  Deploying functions..."
(cd "$DEPLOY_DIR" && npm install --silent 2>/dev/null)
(cd "$DEPLOY_DIR" && npx convex deploy --admin-key "$NODE_A_KEY" --url "$NODE_A_URL" > /dev/null 2>&1)
(cd "$DEPLOY_DIR" && npx convex deploy --admin-key "$NODE_B_KEY" --url "$NODE_B_URL" > /dev/null 2>&1)
echo "  Functions deployed to both nodes."

# --- Helpers ---

mutate() {
    curl -sf "$1/api/mutation" \
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

jval() { python3 -c "import sys,json; print(int(json.load(sys.stdin)['value']['$1']))" <<< "$2"; }
jtotal() { python3 -c "import sys,json; print(int(json.load(sys.stdin)['value']))" <<< "$1"; }

# --- Baseline: capture existing counts before tests ---

echo ""
echo -e "${BOLD}Capturing baseline counts...${NC}"
BASELINE=$(query_api "$NODE_A_URL" "$NODE_A_KEY" "messages:dashboard" 2>/dev/null || echo '{"value":{"messages":0,"users":0,"projects":0,"tasks":0}}')
BASE_M=$(jval messages "$BASELINE" 2>/dev/null || echo 0)
BASE_U=$(jval users "$BASELINE" 2>/dev/null || echo 0)
BASE_P=$(jval projects "$BASELINE" 2>/dev/null || echo 0)
BASE_T=$(jval tasks "$BASELINE" 2>/dev/null || echo 0)
BASE_BUDGET=$(jtotal "$(query_api "$NODE_A_URL" "$NODE_A_KEY" "messages:totalBudget" 2>/dev/null || echo '{"value":0}')" 2>/dev/null || echo 0)
echo "  Baseline: msgs=$BASE_M users=$BASE_U proj=$BASE_P tasks=$BASE_T budget=\$$BASE_BUDGET"

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
EXPECTED_COMP=$((EXPECTED + SALARY_DELTA))

[ "$CA_TOTAL" -eq "$EXPECTED_COMP" ] \
    && pass "Node A cross-table total: \$$CA_TOTAL (salaries + budgets)" \
    || fail "Node A cross-table invariant" "got \$$CA_TOTAL, expected \$$EXPECTED_COMP"

[ "$CB_TOTAL" -eq "$EXPECTED_COMP" ] \
    && pass "Node B cross-table total: \$$CB_TOTAL" \
    || fail "Node B cross-table invariant" "got \$$CB_TOTAL, expected \$$EXPECTED_COMP"

[ "$CA_TOTAL" -eq "$CB_TOTAL" ] \
    && pass "Cross-node multi-table invariant: both agree on \$$CA_TOTAL" \
    || fail "Cross-node multi-table invariant violated" "A=\$$CA_TOTAL vs B=\$$CB_TOTAL"

# ============================================================
echo ""
echo -e "${BOLD}Test 4: Partition Enforcement (Vitess Single Mode)${NC}"
# ============================================================

# Node A → projects (partition 1): must fail
R=$(curl -s "$NODE_A_URL/api/mutation" -H "Authorization: Convex $NODE_A_KEY" -H "Content-Type: application/json" \
    -d '{"path":"messages:createProject","args":{"name":"X","status":"x","budget":0}}')
echo "$R" | grep -q "InternalServerError" \
    && pass "Node A rejected write to 'projects' (partition 1)" \
    || fail "Node A should reject projects write" "$R"

# Node B → messages (partition 0): must fail
R=$(curl -s "$NODE_B_URL/api/mutation" -H "Authorization: Convex $NODE_B_KEY" -H "Content-Type: application/json" \
    -d '{"path":"messages:send","args":{"text":"x","author":"x","channel":"x"}}')
echo "$R" | grep -q "InternalServerError" \
    && pass "Node B rejected write to 'messages' (partition 0)" \
    || fail "Node B should reject messages write" "$R"

# Node A → tasks (partition 1): must fail
R=$(curl -s "$NODE_A_URL/api/mutation" -H "Authorization: Convex $NODE_A_KEY" -H "Content-Type: application/json" \
    -d '{"path":"messages:createTask","args":{"title":"x","assignee":"x","project":"x","status":"x"}}')
echo "$R" | grep -q "InternalServerError" \
    && pass "Node A rejected write to 'tasks' (partition 1)" \
    || fail "Node A should reject tasks write" "$R"

# Node B → users (partition 0): must fail
R=$(curl -s "$NODE_B_URL/api/mutation" -H "Authorization: Convex $NODE_B_KEY" -H "Content-Type: application/json" \
    -d '{"path":"messages:createUser","args":{"name":"x","role":"x"}}')
echo "$R" | grep -q "InternalServerError" \
    && pass "Node B rejected write to 'users' (partition 0)" \
    || fail "Node B should reject users write" "$R"

# Verify no phantom data — counts should match post-test1 values
sleep 1
DC=$(query_api "$NODE_A_URL" "$NODE_A_KEY" "messages:dashboard")
CP=$(jval projects "$DC"); CM=$(jval messages "$DC")
[ "$CP" -eq "$AP" ] && [ "$CM" -eq "$AM" ] \
    && pass "No phantom data from rejected writes (proj=$CP msgs=$CM unchanged)" \
    || fail "Phantom data detected" "proj=$CP (expected $AP) msgs=$CM (expected $AM)"

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

# Override readLatestSeq to pass key arg
query_with_args() {
    curl -sf "$1/api/query" \
        -H "Authorization: Convex $2" \
        -H "Content-Type: application/json" \
        -d "{\"path\":\"$3\",\"args\":$4}"
}

# ============================================================
echo ""
echo -e "${BOLD}Test 7: Node Restart Recovery (TiDB kill -9 / CockroachDB nemesis)${NC}"
# ============================================================

# Capture pre-restart state
PRE=$(query_api "$NODE_A_URL" "$NODE_A_KEY" "messages:dashboard")
PRE_M=$(jval messages "$PRE")

# Restart Node B
echo "  Restarting Node B..."
docker restart docker-node-b-1 > /dev/null 2>&1

# Write to Node A while Node B is restarting
mutate "$NODE_A_URL" "$NODE_A_KEY" "messages:send" \
    '{"text":"during-restart","author":"recovery-test","channel":"test"}' > /dev/null

# Wait for Node B to come back healthy
echo "  Waiting for Node B to recover..."
for attempt in $(seq 1 30); do
    if curl -sf "http://127.0.0.1:3220/version" > /dev/null 2>&1; then
        break
    fi
    sleep 1
done

# Re-generate Node B key (may have changed on restart)
NODE_B_KEY=$(docker exec docker-node-b-1 ./generate_admin_key.sh 2>&1 | tail -1)

# Re-deploy functions to Node B (modules are in-memory)
echo "  Re-deploying functions to Node B..."
(cd "$DEPLOY_DIR" && npx convex deploy --admin-key "$NODE_B_KEY" --url "$NODE_B_URL" > /dev/null 2>&1)

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
