#!/usr/bin/env bash
#
# Raft Failover Integration Tests
#
# Tests automatic leader election and failover using 3 Docker containers
# running a Raft consensus group (tikv/raft-rs).
#
# Based on:
#   - CockroachDB roachtest failover/non-system/crash
#   - TiKV fail-rs chaos testing
#   - YugabyteDB Jepsen nightly resilience benchmarks
#
# Prerequisites:
#   docker compose --profile cluster up
#
# Usage:
#   cd self-hosted/docker && ./test-raft-failover.sh

set -euo pipefail

NODE_A_URL="http://127.0.0.1:3210"
NODE_B_URL="http://127.0.0.1:3220"
NODE_C_URL="http://127.0.0.1:3230"

RED='\033[0;31m'
GREEN='\033[0;32m'
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

for name in docker-node-p0a-1 docker-node-p0b-1 docker-node-p0c-1; do
    if ! docker inspect "$name" > /dev/null 2>&1; then
        echo -e "${RED}Container $name not running. Start:${NC}"
        echo "  docker compose --profile cluster up"
        exit 1
    fi
done
echo "  All 3 Raft nodes running."

# Generate admin keys for all nodes.
KEY_A=$(docker exec docker-node-p0a-1 ./generate_admin_key.sh 2>&1 | tail -1)
KEY_B=$(docker exec docker-node-p0b-1 ./generate_admin_key.sh 2>&1 | tail -1)
KEY_C=$(docker exec docker-node-p0c-1 ./generate_admin_key.sh 2>&1 | tail -1)
echo "  Admin keys generated."

# Deploy functions to Node A (leader deploys, followers replicate).
DEPLOY_DIR=$(mktemp -d)
mkdir -p "$DEPLOY_DIR/convex"
cat > "$DEPLOY_DIR/package.json" << 'EOF'
{ "name": "raft-test", "version": "1.0.0", "dependencies": { "convex": "^1" } }
EOF

cat > "$DEPLOY_DIR/convex/messages.ts" << 'TSEOF'
import { mutation, query } from "./_generated/server";
import { v } from "convex/values";

export const send = mutation({
  args: { text: v.string() },
  handler: async (ctx, args) => {
    await ctx.db.insert("messages", { text: args.text, timestamp: Date.now() });
  },
});

export const count = query({
  handler: async (ctx) => {
    const msgs = await ctx.db.query("messages").collect();
    return msgs.length;
  },
});
TSEOF

echo "  Deploying functions..."
(cd "$DEPLOY_DIR" && npm install --silent 2>/dev/null)
(cd "$DEPLOY_DIR" && npx convex deploy --admin-key "$KEY_A" --url "$NODE_A_URL" > /dev/null 2>&1)
echo "  Functions deployed."

# --- Helpers ---

mutate() {
    curl -sf "$1/api/mutation" \
        -H "Authorization: Convex $2" \
        -H "Content-Type: application/json" \
        -d "{\"path\":\"messages:send\",\"args\":{\"text\":\"$3\"}}"
}

count_msgs() {
    curl -sf "$1/api/query" \
        -H "Authorization: Convex $2" \
        -H "Content-Type: application/json" \
        -d '{"path":"messages:count","args":{}}' \
        | python3 -c "import sys,json; print(int(json.load(sys.stdin)['value']))"
}

# ============================================================
echo ""
echo -e "${BOLD}Test 1: All 3 Nodes Healthy${NC}"
# ============================================================

curl -sf "$NODE_A_URL/version" > /dev/null && pass "Node A (port 3210) healthy" || fail "Node A down"
curl -sf "$NODE_B_URL/version" > /dev/null && pass "Node B (port 3220) healthy" || fail "Node B down"
curl -sf "$NODE_C_URL/version" > /dev/null && pass "Node C (port 3230) healthy" || fail "Node C down"

# ============================================================
echo ""
echo -e "${BOLD}Test 2: Write to Leader${NC}"
# ============================================================

# Try writing to Node A (likely leader — lowest Raft ID).
R=$(mutate "$NODE_A_URL" "$KEY_A" "raft-test-1" 2>&1 || echo "FAIL")
if echo "$R" | grep -q "success"; then
    pass "Write to Node A succeeded"
else
    # Node A might not be leader. Try B and C.
    R=$(mutate "$NODE_B_URL" "$KEY_B" "raft-test-1" 2>&1 || echo "FAIL")
    if echo "$R" | grep -q "success"; then
        pass "Write to Node B succeeded (Node B is leader)"
    else
        R=$(mutate "$NODE_C_URL" "$KEY_C" "raft-test-1" 2>&1 || echo "FAIL")
        if echo "$R" | grep -q "success"; then
            pass "Write to Node C succeeded (Node C is leader)"
        else
            fail "No node accepted writes" "All 3 rejected"
        fi
    fi
fi

# ============================================================
echo ""
echo -e "${BOLD}Test 3: Read from All Nodes${NC}"
# ============================================================

sleep 2

CA=$(count_msgs "$NODE_A_URL" "$KEY_A" 2>/dev/null || echo 0)
CB=$(count_msgs "$NODE_B_URL" "$KEY_B" 2>/dev/null || echo 0)
CC=$(count_msgs "$NODE_C_URL" "$KEY_C" 2>/dev/null || echo 0)

[ "$CA" -ge 1 ] && pass "Node A sees data: $CA messages" || fail "Node A has no data" "$CA"
[ "$CA" -eq "$CB" ] && [ "$CB" -eq "$CC" ] \
    && pass "All 3 nodes agree: $CA messages" \
    || fail "Nodes disagree" "A=$CA B=$CB C=$CC"

# ============================================================
echo ""
echo -e "${BOLD}Test 4: Kill Leader, Verify Failover${NC}"
# ============================================================

PRE_KILL=$CA

echo "  Killing Node A (likely leader)..."
docker kill docker-node-p0a-1 > /dev/null 2>&1

sleep 5

# Try writing to remaining nodes.
WRITE_OK=false
for url_key in "$NODE_B_URL $KEY_B" "$NODE_C_URL $KEY_C"; do
    URL=$(echo "$url_key" | cut -d' ' -f1)
    KEY=$(echo "$url_key" | cut -d' ' -f2)
    R=$(mutate "$URL" "$KEY" "after-kill" 2>&1 || echo "FAIL")
    if echo "$R" | grep -q "success"; then
        WRITE_OK=true
        pass "Failover: writes accepted on $URL after leader kill"
        break
    fi
done

$WRITE_OK || fail "No node accepted writes after leader kill"

# Verify data on surviving nodes.
sleep 2
CB2=$(count_msgs "$NODE_B_URL" "$KEY_B" 2>/dev/null || echo 0)
CC2=$(count_msgs "$NODE_C_URL" "$KEY_C" 2>/dev/null || echo 0)

[ "$CB2" -gt "$PRE_KILL" ] || [ "$CC2" -gt "$PRE_KILL" ] \
    && pass "Data written after failover: B=$CB2 C=$CC2 (pre-kill=$PRE_KILL)" \
    || fail "No new data after failover" "B=$CB2 C=$CC2"

# ============================================================
echo ""
echo -e "${BOLD}Test 5: Restart Killed Node, Verify Rejoin${NC}"
# ============================================================

echo "  Restarting Node A..."
docker start docker-node-p0a-1 > /dev/null 2>&1

echo "  Waiting for recovery..."
for attempt in $(seq 1 30); do
    curl -sf "$NODE_A_URL/version" > /dev/null 2>&1 && break
    sleep 1
done

KEY_A=$(docker exec docker-node-p0a-1 ./generate_admin_key.sh 2>&1 | tail -1)
(cd "$DEPLOY_DIR" && npx convex deploy --admin-key "$KEY_A" --url "$NODE_A_URL" > /dev/null 2>&1)

sleep 4

CA3=$(count_msgs "$NODE_A_URL" "$KEY_A" 2>/dev/null || echo 0)
CB3=$(count_msgs "$NODE_B_URL" "$KEY_B" 2>/dev/null || echo 0)

[ "$CA3" -ge "$PRE_KILL" ] \
    && pass "Node A recovered: sees $CA3 messages (>=$PRE_KILL)" \
    || fail "Node A lost data" "sees $CA3, expected >=$PRE_KILL"

[ "$CA3" -eq "$CB3" ] \
    && pass "All nodes converged after rejoin: $CA3 messages" \
    || fail "Nodes diverged after rejoin" "A=$CA3 B=$CB3"

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
