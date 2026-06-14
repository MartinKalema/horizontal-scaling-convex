#!/usr/bin/env bash
#
# Commit hot-path benchmark for the partitioned Docker cluster.
#
# This is intentionally separate from test-write-scaling.sh: the correctness
# harness proves invariants, while this script gives a repeatable latency and
# throughput snapshot for #165-style commit-path changes.
#
# Usage:
#   cd self-hosted/docker
#   ./benchmark-commit-hot-path.sh
#
# Optional environment:
#   OPS=200 CONCURRENCY=8 ./benchmark-commit-hot-path.sh
#   SCENARIOS=local_p0,local_p1,two_pc ./benchmark-commit-hot-path.sh

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
REPO_ROOT="$(cd "$SCRIPT_DIR/../.." && pwd)"

NODE_P0A_URL="${NODE_P0A_URL:-http://127.0.0.1:3210}"
NODE_P1A_URL="${NODE_P1A_URL:-http://127.0.0.1:3310}"
OPS="${OPS:-100}"
CONCURRENCY="${CONCURRENCY:-8}"
WARMUP="${WARMUP:-10}"
SCENARIOS="${SCENARIOS:-local_p0,local_p1,two_pc}"

RED='\033[0;31m'
GREEN='\033[0;32m'
BOLD='\033[1m'
NC='\033[0m'

require_container() {
    local name="$1"
    if ! docker inspect "$name" > /dev/null 2>&1; then
        echo -e "${RED}Container $name not running. Start the cluster first:${NC}"
        echo "  docker compose --profile cluster up"
        exit 1
    fi
}

echo ""
echo -e "${BOLD}Commit Hot Path Benchmark${NC}"
echo "  ops=$OPS concurrency=$CONCURRENCY warmup=$WARMUP scenarios=$SCENARIOS"
echo ""

require_container docker-node-p0a-1
require_container docker-node-p1a-1

NODE_P0A_KEY=$(docker exec docker-node-p0a-1 ./generate_admin_key.sh 2>&1 | tail -1)
NODE_P1A_KEY=$(docker exec docker-node-p1a-1 ./generate_admin_key.sh 2>&1 | tail -1)

DEPLOY_DIR=$(mktemp -d)
cleanup() {
    rm -rf "$DEPLOY_DIR"
}
trap cleanup EXIT

mkdir -p "$DEPLOY_DIR/convex"
cat > "$DEPLOY_DIR/package.json" << EOF
{ "name": "commit-hot-path-benchmark", "version": "1.0.0", "dependencies": { "convex": "file:$REPO_ROOT/npm-packages/convex" } }
EOF

cat > "$DEPLOY_DIR/convex/messages.ts" << 'TSEOF'
import { mutation, query } from "./_generated/server";
import { v } from "convex/values";

export const benchMessage = mutation({
  args: { run: v.string(), i: v.number() },
  handler: async (ctx, args) => {
    await ctx.db.insert("messages", {
      text: `bench-message-${args.run}-${args.i}`,
      author: "commit-benchmark",
      channel: args.run,
      timestamp: Date.now(),
    });
  },
});

export const benchTask = mutation({
  args: { run: v.string(), i: v.number() },
  handler: async (ctx, args) => {
    await ctx.db.insert("tasks", {
      title: `bench-task-${args.run}-${args.i}`,
      assignee: "commit-benchmark",
      project: args.run,
      status: "done",
      createdAt: Date.now(),
    });
  },
});

export const benchCrossPartition = mutation({
  args: { run: v.string(), i: v.number() },
  handler: async (ctx, args) => {
    await ctx.db.insert("messages", {
      text: `bench-2pc-message-${args.run}-${args.i}`,
      author: "commit-benchmark-2pc",
      channel: args.run,
      timestamp: Date.now(),
    });
    await ctx.db.insert("tasks", {
      title: `bench-2pc-task-${args.run}-${args.i}`,
      assignee: "commit-benchmark-2pc",
      project: args.run,
      status: "done",
      createdAt: Date.now(),
    });
  },
});

export const benchCounts = query({
  args: { run: v.string() },
  handler: async (ctx, args) => {
    const messages = await ctx.db.query("messages").collect();
    const tasks = await ctx.db.query("tasks").collect();
    return {
      messages: messages.filter((m: any) => m.channel === args.run).length,
      tasks: tasks.filter((t: any) => t.project === args.run).length,
    };
  },
});
TSEOF

echo "  Deploying benchmark functions..."
(cd "$DEPLOY_DIR" && npm install --silent 2>/dev/null)
(cd "$DEPLOY_DIR" && npx convex deploy --admin-key "$NODE_P0A_KEY" --url "$NODE_P0A_URL" > /dev/null 2>&1)
(cd "$DEPLOY_DIR" && npx convex deploy --admin-key "$NODE_P1A_KEY" --url "$NODE_P1A_URL" > /dev/null 2>&1)
echo -e "  ${GREEN}Functions deployed.${NC}"
echo ""

python3 - "$NODE_P0A_URL" "$NODE_P0A_KEY" "$NODE_P1A_URL" "$NODE_P1A_KEY" "$OPS" "$CONCURRENCY" "$WARMUP" "$SCENARIOS" << 'PYEOF'
import concurrent.futures
import json
import math
import statistics
import sys
import time
import urllib.request
import uuid

p0_url, p0_key, p1_url, p1_key, ops_s, concurrency_s, warmup_s, scenarios_s = sys.argv[1:]
ops = int(ops_s)
concurrency = int(concurrency_s)
warmup = int(warmup_s)
scenarios = [s.strip() for s in scenarios_s.split(",") if s.strip()]
run_id = f"commit-bench-{uuid.uuid4().hex[:10]}"

SCENARIO_CONFIG = {
    "local_p0": (p0_url, p0_key, "messages:benchMessage"),
    "local_p1": (p1_url, p1_key, "messages:benchTask"),
    "two_pc": (p0_url, p0_key, "messages:benchCrossPartition"),
}

def post_json(url, key, endpoint, payload):
    request = urllib.request.Request(
        f"{url}{endpoint}",
        data=json.dumps(payload).encode("utf-8"),
        headers={
            "Authorization": f"Convex {key}",
            "Content-Type": "application/json",
        },
        method="POST",
    )
    with urllib.request.urlopen(request, timeout=30) as response:
        body = response.read()
    parsed = json.loads(body)
    if parsed.get("status") == "error":
        raise RuntimeError(parsed)
    return parsed

def mutate(url, key, path, i):
    start = time.perf_counter_ns()
    try:
        post_json(url, key, "/api/mutation", {"path": path, "args": {"run": run_id, "i": i}})
        ok = True
        error = ""
    except Exception as exc:
        ok = False
        error = repr(exc)
    elapsed_ms = (time.perf_counter_ns() - start) / 1_000_000
    return elapsed_ms, ok, error

def percentile(values, pct):
    if not values:
        return float("nan")
    ordered = sorted(values)
    index = max(0, math.ceil(len(ordered) * pct / 100) - 1)
    return ordered[index]

print("scenario,ops,concurrency,errors,total_sec,throughput_ops_per_sec,p50_ms,p95_ms,p99_ms")

expected_messages = 0
expected_tasks = 0

for scenario in scenarios:
    if scenario not in SCENARIO_CONFIG:
        raise SystemExit(f"unknown scenario {scenario!r}; expected one of {sorted(SCENARIO_CONFIG)}")
    url, key, path = SCENARIO_CONFIG[scenario]

    for i in range(warmup):
        mutate(url, key, path, -i - 1)

    scenario_writes = ops + warmup
    if scenario == "local_p0":
        expected_messages += scenario_writes
    elif scenario == "local_p1":
        expected_tasks += scenario_writes
    elif scenario == "two_pc":
        expected_messages += scenario_writes
        expected_tasks += scenario_writes

    started = time.perf_counter()
    latencies = []
    errors = []
    with concurrent.futures.ThreadPoolExecutor(max_workers=concurrency) as executor:
        futures = [executor.submit(mutate, url, key, path, i) for i in range(ops)]
        for future in concurrent.futures.as_completed(futures):
            elapsed_ms, ok, error = future.result()
            latencies.append(elapsed_ms)
            if not ok:
                errors.append(error)
    total_sec = time.perf_counter() - started
    throughput = ops / total_sec if total_sec else float("inf")
    p50 = statistics.median(latencies) if latencies else float("nan")
    p95 = percentile(latencies, 95)
    p99 = percentile(latencies, 99)

    print(
        f"{scenario},{ops},{concurrency},{len(errors)},"
        f"{total_sec:.3f},{throughput:.1f},{p50:.2f},{p95:.2f},{p99:.2f}"
    )
    if errors:
        print(f"  first_error={errors[0]}", file=sys.stderr)

counts = None
for _ in range(20):
    counts = post_json(
        p0_url,
        p0_key,
        "/api/query",
        {"path": "messages:benchCounts", "args": {"run": run_id}},
    )["value"]
    if counts["messages"] >= expected_messages and counts["tasks"] >= expected_tasks:
        break
    time.sleep(0.5)

print(
    f"verification_run={run_id} "
    f"messages={int(counts['messages'])}/{expected_messages} "
    f"tasks={int(counts['tasks'])}/{expected_tasks}"
)
if counts["messages"] < expected_messages or counts["tasks"] < expected_tasks:
    raise SystemExit("benchmark verification failed: not all writes became visible")
PYEOF
