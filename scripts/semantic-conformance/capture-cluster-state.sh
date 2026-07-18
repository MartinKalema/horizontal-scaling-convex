#!/usr/bin/env bash

set -u

REPO_ROOT=$(cd "$(dirname "$0")/../.." && pwd)
COMPOSE_ROOT="$REPO_ROOT/self-hosted/docker"
ARTIFACT_DIR=${1:-"$REPO_ROOT/artifacts/semantic-conformance/cluster-state"}
SERVICES=(node-p0a node-p0b node-p0c node-p1a node-p1b node-p1c)
COMPOSE=(docker compose --project-directory "$COMPOSE_ROOT" -f "$COMPOSE_ROOT/docker-compose.yml" --profile cluster)

mkdir -p "$ARTIFACT_DIR"
cd "$REPO_ROOT"

"${COMPOSE[@]}" ps --all --format json \
  > "$ARTIFACT_DIR/compose-ps.json" 2> "$ARTIFACT_DIR/compose-ps.error" || true

for service in "${SERVICES[@]}"; do
  container_id=$("${COMPOSE[@]}" ps -q "$service" 2>/dev/null || true)
  if [[ -z "$container_id" ]]; then
    printf 'service %s has no container\n' "$service" \
      > "$ARTIFACT_DIR/$service-missing.txt"
    continue
  fi

  docker inspect \
    --format '{{json .State}}' "$container_id" \
    > "$ARTIFACT_DIR/$service-state.json" 2>&1 || true
  docker inspect \
    --format '{{json .NetworkSettings.Networks}}' "$container_id" \
    > "$ARTIFACT_DIR/$service-networks.json" 2>&1 || true
  docker logs --timestamps "$container_id" \
    > "$ARTIFACT_DIR/$service.log" 2>&1 || true
done

echo "Cluster diagnostics captured in $ARTIFACT_DIR"
