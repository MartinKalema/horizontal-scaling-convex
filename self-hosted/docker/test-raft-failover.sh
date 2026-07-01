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
#   ./start-cluster.sh
#
# Usage:
#   cd self-hosted/docker && ./test-raft-failover.sh

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
REPO_ROOT="$(cd "$SCRIPT_DIR/../.." && pwd)"

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
        echo "  ./start-cluster.sh"
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
cat > "$DEPLOY_DIR/package.json" << EOF
{ "name": "raft-test", "version": "1.0.0", "dependencies": { "convex": "file:$REPO_ROOT/npm-packages/convex" } }
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

export const countByPrefix = query({
  args: { prefix: v.string() },
  handler: async (ctx, args) => {
    const msgs = await ctx.db.query("messages").collect();
    return msgs.filter((msg) => msg.text.startsWith(args.prefix)).length;
  },
});
TSEOF

echo "  Deploying functions..."
(cd "$DEPLOY_DIR" && npm install --silent 2>/dev/null)
(cd "$DEPLOY_DIR" && npx convex deploy --admin-key "$KEY_A" --url "$NODE_A_URL" > /dev/null 2>&1)
echo "  Functions deployed."

# --- Helpers ---

mutate() {
    curl -sf -m 20 "$1/api/mutation" \
        -H "Authorization: Convex $2" \
        -H "Content-Type: application/json" \
        -d "{\"path\":\"messages:send\",\"args\":{\"text\":\"$3\"}}"
}

count_msgs() {
    curl -sf -m 20 "$1/api/query" \
        -H "Authorization: Convex $2" \
        -H "Content-Type: application/json" \
        -d '{"path":"messages:count","args":{}}' \
        | python3 -c "import sys,json; print(int(json.load(sys.stdin)['value']))"
}

count_prefix() {
    curl -sf -m 20 "$1/api/query" \
        -H "Authorization: Convex $2" \
        -H "Content-Type: application/json" \
        -d "{\"path\":\"messages:countByPrefix\",\"args\":{\"prefix\":\"$3\"}}" \
        | python3 -c "import sys,json; print(int(json.load(sys.stdin)['value']))"
}

wait_for_node() {
    local url="$1"
    for _ in $(seq 1 45); do
        curl -sf "$url/version" > /dev/null 2>&1 && return 0
        sleep 1
    done
    return 1
}

refresh_keys() {
    KEY_A=$(docker exec docker-node-p0a-1 ./generate_admin_key.sh 2>&1 | tail -1)
    KEY_B=$(docker exec docker-node-p0b-1 ./generate_admin_key.sh 2>&1 | tail -1)
    KEY_C=$(docker exec docker-node-p0c-1 ./generate_admin_key.sh 2>&1 | tail -1)
}

wait_for_prefix_count() {
    local url="$1"
    local key="$2"
    local prefix="$3"
    local expected="$4"
    for _ in $(seq 1 30); do
        local count
        count=$(count_prefix "$url" "$key" "$prefix" 2>/dev/null || echo -1)
        [ "$count" -eq "$expected" ] && return 0
        sleep 1
    done
    return 1
}

write_to_either_survivor() {
    local prefix="$1"
    local text="$2"
    shift 2
    local endpoints=("$@")
    local last_response=""
    # Leader changes can leave a short no-leader window. Treat availability as
    # bounded retry success instead of a single unlucky sample during election.
    for _ in $(seq 1 30); do
        set -- "${endpoints[@]}"
        while [ "$#" -gt 0 ]; do
            local url="$1"
            local key="$2"
            shift 2
            local response
            response=$(mutate "$url" "$key" "$prefix-$text" 2>&1 || echo "FAIL")
            if echo "$response" | grep -q "success"; then
                return 0
            fi
            last_response="$response"
        done
        sleep 1
    done
    [ -n "$last_response" ] && echo "$last_response" >&2
    return 1
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
echo -e "${BOLD}Test 6: Follower Entrypoint Write Routing${NC}"
# ============================================================

PREFIX="raft-follower-route-$(date +%s)"
ACCEPTED=0
for url_key_name in \
    "$NODE_A_URL $KEY_A p0a" \
    "$NODE_B_URL $KEY_B p0b" \
    "$NODE_C_URL $KEY_C p0c"; do
    URL=$(echo "$url_key_name" | cut -d' ' -f1)
    KEY=$(echo "$url_key_name" | cut -d' ' -f2)
    NAME=$(echo "$url_key_name" | cut -d' ' -f3)
    R=$(mutate "$URL" "$KEY" "$PREFIX-$NAME" 2>&1 || echo "FAIL")
    if echo "$R" | grep -q "success"; then
        ACCEPTED=$((ACCEPTED + 1))
    fi
done

[ "$ACCEPTED" -eq 3 ] \
    && pass "All three Raft entrypoints accepted writes via leader/follower routing" \
    || fail "Not all Raft entrypoints accepted routed writes" "accepted=$ACCEPTED expected=3"

if wait_for_prefix_count "$NODE_A_URL" "$KEY_A" "$PREFIX" 3 \
    && wait_for_prefix_count "$NODE_B_URL" "$KEY_B" "$PREFIX" 3 \
    && wait_for_prefix_count "$NODE_C_URL" "$KEY_C" "$PREFIX" 3; then
    pass "Follower-routed writes converged on all three nodes"
else
    CA_ROUTE=$(count_prefix "$NODE_A_URL" "$KEY_A" "$PREFIX" 2>/dev/null || echo -1)
    CB_ROUTE=$(count_prefix "$NODE_B_URL" "$KEY_B" "$PREFIX" 2>/dev/null || echo -1)
    CC_ROUTE=$(count_prefix "$NODE_C_URL" "$KEY_C" "$PREFIX" 2>/dev/null || echo -1)
    fail "Follower-routed writes did not converge" "A=$CA_ROUTE B=$CB_ROUTE C=$CC_ROUTE"
fi

# ============================================================
echo ""
echo -e "${BOLD}Test 7: Majority Continues While One Follower Is Offline${NC}"
# ============================================================

PREFIX="raft-one-follower-offline-$(date +%s)"
echo "  Stopping Node C..."
docker stop docker-node-p0c-1 > /dev/null 2>&1
sleep 5

ACCEPTED=0
for i in $(seq 1 6); do
    if write_to_either_survivor "$PREFIX" "write-$i" \
        "$NODE_A_URL" "$KEY_A" \
        "$NODE_B_URL" "$KEY_B"; then
        ACCEPTED=$((ACCEPTED + 1))
    fi
done

[ "$ACCEPTED" -eq 6 ] \
    && pass "Majority accepted writes while Node C was offline" \
    || fail "Majority did not accept all writes while Node C was offline" "accepted=$ACCEPTED expected=6"

if wait_for_prefix_count "$NODE_A_URL" "$KEY_A" "$PREFIX" "$ACCEPTED" \
    && wait_for_prefix_count "$NODE_B_URL" "$KEY_B" "$PREFIX" "$ACCEPTED"; then
    pass "Surviving majority agreed while follower was offline"
else
    CA_OFFLINE=$(count_prefix "$NODE_A_URL" "$KEY_A" "$PREFIX" 2>/dev/null || echo -1)
    CB_OFFLINE=$(count_prefix "$NODE_B_URL" "$KEY_B" "$PREFIX" 2>/dev/null || echo -1)
    fail "Surviving majority did not agree while follower was offline" "A=$CA_OFFLINE B=$CB_OFFLINE expected=$ACCEPTED"
fi

echo "  Restarting Node C..."
docker start docker-node-p0c-1 > /dev/null 2>&1
wait_for_node "$NODE_C_URL" || fail "Node C did not come back after follower-offline test"
refresh_keys
if wait_for_prefix_count "$NODE_C_URL" "$KEY_C" "$PREFIX" "$ACCEPTED"; then
    pass "Restarted follower caught up with offline-window writes"
else
    CC_OFFLINE=$(count_prefix "$NODE_C_URL" "$KEY_C" "$PREFIX" 2>/dev/null || echo -1)
    fail "Restarted follower did not catch up" "C=$CC_OFFLINE expected=$ACCEPTED"
fi

# ============================================================
echo ""
echo -e "${BOLD}Test 8: No-Quorum Writes Fail Closed${NC}"
# ============================================================

PREFIX="raft-no-quorum-$(date +%s)"
echo "  Stopping Nodes B and C to remove quorum..."
docker stop docker-node-p0b-1 docker-node-p0c-1 > /dev/null 2>&1
sleep 7

R=$(mutate "$NODE_A_URL" "$KEY_A" "$PREFIX-should-not-commit" 2>&1 || echo "FAIL")
if echo "$R" | grep -q "success"; then
    fail "Single surviving node accepted a write without quorum" "$R"
else
    pass "Single surviving node rejected writes without quorum"
fi

echo "  Restarting Nodes B and C..."
docker start docker-node-p0b-1 docker-node-p0c-1 > /dev/null 2>&1
wait_for_node "$NODE_B_URL" || fail "Node B did not come back after no-quorum test"
wait_for_node "$NODE_C_URL" || fail "Node C did not come back after no-quorum test"
refresh_keys
sleep 5

CA_NO_QUORUM=$(count_prefix "$NODE_A_URL" "$KEY_A" "$PREFIX" 2>/dev/null || echo -1)
CB_NO_QUORUM=$(count_prefix "$NODE_B_URL" "$KEY_B" "$PREFIX" 2>/dev/null || echo -1)
CC_NO_QUORUM=$(count_prefix "$NODE_C_URL" "$KEY_C" "$PREFIX" 2>/dev/null || echo -1)
if [ "$CA_NO_QUORUM" -eq 0 ] && [ "$CB_NO_QUORUM" -eq 0 ] && [ "$CC_NO_QUORUM" -eq 0 ]; then
    pass "Rejected no-quorum write did not leak after recovery"
else
    fail "No-quorum write leaked into recovered state" "A=$CA_NO_QUORUM B=$CB_NO_QUORUM C=$CC_NO_QUORUM"
fi

# ============================================================
echo ""
echo -e "${BOLD}Test 9: Rolling Raft Restarts Preserve Availability${NC}"
# ============================================================

PREFIX="raft-rolling-restart-$(date +%s)"
ROLLING_ACCEPTED=0

echo "  Rolling restart: Node A down..."
docker stop docker-node-p0a-1 > /dev/null 2>&1
sleep 5
if write_to_either_survivor "$PREFIX" "while-a-down" \
    "$NODE_B_URL" "$KEY_B" \
    "$NODE_C_URL" "$KEY_C"; then
    ROLLING_ACCEPTED=$((ROLLING_ACCEPTED + 1))
    pass "Write accepted while Node A was down"
else
    fail "No write accepted while Node A was down"
fi
docker start docker-node-p0a-1 > /dev/null 2>&1
wait_for_node "$NODE_A_URL" || fail "Node A did not recover during rolling restart"
refresh_keys
sleep 3

echo "  Rolling restart: Node B down..."
docker stop docker-node-p0b-1 > /dev/null 2>&1
sleep 5
if write_to_either_survivor "$PREFIX" "while-b-down" \
    "$NODE_A_URL" "$KEY_A" \
    "$NODE_C_URL" "$KEY_C"; then
    ROLLING_ACCEPTED=$((ROLLING_ACCEPTED + 1))
    pass "Write accepted while Node B was down"
else
    fail "No write accepted while Node B was down"
fi
docker start docker-node-p0b-1 > /dev/null 2>&1
wait_for_node "$NODE_B_URL" || fail "Node B did not recover during rolling restart"
refresh_keys
sleep 3

echo "  Rolling restart: Node C down..."
docker stop docker-node-p0c-1 > /dev/null 2>&1
sleep 5
if write_to_either_survivor "$PREFIX" "while-c-down" \
    "$NODE_A_URL" "$KEY_A" \
    "$NODE_B_URL" "$KEY_B"; then
    ROLLING_ACCEPTED=$((ROLLING_ACCEPTED + 1))
    pass "Write accepted while Node C was down"
else
    fail "No write accepted while Node C was down"
fi
docker start docker-node-p0c-1 > /dev/null 2>&1
wait_for_node "$NODE_C_URL" || fail "Node C did not recover during rolling restart"
refresh_keys
sleep 5

if wait_for_prefix_count "$NODE_A_URL" "$KEY_A" "$PREFIX" "$ROLLING_ACCEPTED" \
    && wait_for_prefix_count "$NODE_B_URL" "$KEY_B" "$PREFIX" "$ROLLING_ACCEPTED" \
    && wait_for_prefix_count "$NODE_C_URL" "$KEY_C" "$PREFIX" "$ROLLING_ACCEPTED"; then
    pass "Rolling restart writes converged on all three nodes"
else
    CA_ROLLING=$(count_prefix "$NODE_A_URL" "$KEY_A" "$PREFIX" 2>/dev/null || echo -1)
    CB_ROLLING=$(count_prefix "$NODE_B_URL" "$KEY_B" "$PREFIX" 2>/dev/null || echo -1)
    CC_ROLLING=$(count_prefix "$NODE_C_URL" "$KEY_C" "$PREFIX" 2>/dev/null || echo -1)
    fail "Rolling restart writes did not converge" "A=$CA_ROLLING B=$CB_ROLLING C=$CC_ROLLING expected=$ROLLING_ACCEPTED"
fi

# ============================================================
echo ""
echo -e "${BOLD}Test 10: Lagging Follower Burst Catch-Up${NC}"
# ============================================================

PREFIX="raft-lagging-burst-$(date +%s)"
echo "  Stopping Node C before a burst of writes..."
docker stop docker-node-p0c-1 > /dev/null 2>&1
sleep 5

BURST_ACCEPTED=0
for i in $(seq 1 25); do
    if write_to_either_survivor "$PREFIX" "burst-$i" \
        "$NODE_A_URL" "$KEY_A" \
        "$NODE_B_URL" "$KEY_B"; then
        BURST_ACCEPTED=$((BURST_ACCEPTED + 1))
    fi
done

[ "$BURST_ACCEPTED" -eq 25 ] \
    && pass "Majority accepted burst writes while follower lagged" \
    || fail "Majority did not accept full burst while follower lagged" "accepted=$BURST_ACCEPTED expected=25"

echo "  Restarting Node C and waiting for catch-up..."
docker start docker-node-p0c-1 > /dev/null 2>&1
wait_for_node "$NODE_C_URL" || fail "Node C did not recover after lagging-burst test"
refresh_keys
if wait_for_prefix_count "$NODE_C_URL" "$KEY_C" "$PREFIX" "$BURST_ACCEPTED"; then
    pass "Lagging follower caught up with burst writes"
else
    CC_BURST=$(count_prefix "$NODE_C_URL" "$KEY_C" "$PREFIX" 2>/dev/null || echo -1)
    fail "Lagging follower did not catch up with burst writes" "C=$CC_BURST expected=$BURST_ACCEPTED"
fi

if wait_for_prefix_count "$NODE_A_URL" "$KEY_A" "$PREFIX" "$BURST_ACCEPTED" \
    && wait_for_prefix_count "$NODE_B_URL" "$KEY_B" "$PREFIX" "$BURST_ACCEPTED"; then
    pass "All voters agree after lagging follower catch-up"
else
    CA_BURST=$(count_prefix "$NODE_A_URL" "$KEY_A" "$PREFIX" 2>/dev/null || echo -1)
    CB_BURST=$(count_prefix "$NODE_B_URL" "$KEY_B" "$PREFIX" 2>/dev/null || echo -1)
    CC_BURST=$(count_prefix "$NODE_C_URL" "$KEY_C" "$PREFIX" 2>/dev/null || echo -1)
    fail "Voters diverged after lagging follower catch-up" "A=$CA_BURST B=$CB_BURST C=$CC_BURST expected=$BURST_ACCEPTED"
fi

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
