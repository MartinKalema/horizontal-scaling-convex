#!/usr/bin/env bash
#
# Start the local cluster with a fresh advertised-address namespace.
#
# Compose service names remain stable for the test harness and for the static
# Raft bootstrap peers, but nodes advertise per-run DNS aliases into the
# membership directory. This lets the Docker setup exercise production-style
# endpoint discovery instead of relying on a baked-in NODE_ADDRESSES map.

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"

export CLUSTER_RUN_ID="${CLUSTER_RUN_ID:-run-$(date +%s)-$$}"

echo "Starting cluster with CLUSTER_RUN_ID=$CLUSTER_RUN_ID"
echo "Backend pull policy: ${BACKEND_PULL_POLICY:-missing}"

cd "$SCRIPT_DIR"
docker compose --profile cluster up -d "$@"
