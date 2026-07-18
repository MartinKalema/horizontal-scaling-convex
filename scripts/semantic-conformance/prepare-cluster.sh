#!/usr/bin/env bash

set -euo pipefail

REPO_ROOT=$(cd "$(dirname "$0")/../.." && pwd)
COMPOSE_ROOT="$REPO_ROOT/self-hosted/docker"
ARTIFACT_DIR=${1:-"$REPO_ROOT/artifacts/semantic-conformance/cluster"}
IMAGE=${SEMANTIC_BACKEND_IMAGE:-ghcr.io/martinkalema/convex-horizontal-scaling:backend-latest}
HEALTH_TIMEOUT_SECONDS=${SEMANTIC_CLUSTER_HEALTH_TIMEOUT_SECONDS:-180}
SERVICES=(node-p0a node-p0b node-p0c node-p1a node-p1b node-p1c)
COMPOSE=(docker compose --project-directory "$COMPOSE_ROOT" -f "$COMPOSE_ROOT/docker-compose.yml" --profile cluster)

if ! [[ "$HEALTH_TIMEOUT_SECONDS" =~ ^[1-9][0-9]*$ ]]; then
  echo "SEMANTIC_CLUSTER_HEALTH_TIMEOUT_SECONDS must be a positive integer" >&2
  exit 1
fi

mkdir -p "$ARTIFACT_DIR"
cd "$REPO_ROOT"

revision=${GITHUB_SHA:-$(git rev-parse HEAD)}
created=$(date -u +%Y-%m-%dT%H:%M:%SZ)

"${COMPOSE[@]}" down -v --remove-orphans
docker build \
  --progress=plain \
  --build-arg "VERGEN_GIT_SHA=$revision" \
  --build-arg "VERGEN_GIT_COMMIT_TIMESTAMP=$created" \
  -t "$IMAGE" \
  -f self-hosted/docker-build/Dockerfile.backend \
  .

BACKEND_PULL_POLICY=never "$COMPOSE_ROOT/start-cluster.sh"

expected_image_id=$(docker image inspect --format '{{.Id}}' "$IMAGE")
{
  printf 'revision=%s\n' "$revision"
  printf 'image=%s\n' "$IMAGE"
  printf 'image_id=%s\n' "$expected_image_id"
} > "$ARTIFACT_DIR/image-proof.txt"

for service in "${SERVICES[@]}"; do
  container_id=$("${COMPOSE[@]}" ps -q "$service")
  if [[ -z "$container_id" ]]; then
    echo "Missing backend container for service $service" >&2
    exit 1
  fi
  actual_image_id=$(docker inspect --format '{{.Image}}' "$container_id")
  deadline=$((SECONDS + HEALTH_TIMEOUT_SECONDS))
  while true; do
    state=$(docker inspect --format '{{.State.Status}}' "$container_id")
    health=$(docker inspect --format '{{if .State.Health}}{{.State.Health.Status}}{{else}}{{.State.Status}}{{end}}' "$container_id")
    if [[ "$health" == "healthy" ]]; then
      break
    fi
    if [[ "$state" == "exited" || "$state" == "dead" ]]; then
      echo "$service entered terminal state $state before becoming healthy" >&2
      exit 1
    fi
    if ((SECONDS >= deadline)); then
      echo "$service did not become healthy within ${HEALTH_TIMEOUT_SECONDS}s (state=$state health=$health)" >&2
      exit 1
    fi
    sleep 2
  done
  printf '%s container=%s image_id=%s health=%s\n' \
    "$service" "$container_id" "$actual_image_id" "$health" \
    | tee -a "$ARTIFACT_DIR/image-proof.txt"
  if [[ "$actual_image_id" != "$expected_image_id" ]]; then
    echo "$service is not running the locally built image" >&2
    exit 1
  fi
done

echo "All six backend containers are healthy on local image $expected_image_id"
