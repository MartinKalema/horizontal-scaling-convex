#!/usr/bin/env bash
#
# Build the backend image with git provenance baked into both the binary and
# the OCI image labels.

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
REPO_ROOT="$(cd "$SCRIPT_DIR/../.." && pwd)"
IMAGE="${1:-ghcr.io/martinkalema/convex-horizontal-scaling:backend-latest}"

GIT_SHA="$(git -C "$REPO_ROOT" rev-parse HEAD)"
GIT_COMMIT_TIMESTAMP="$(git -C "$REPO_ROOT" show -s --format=%cI HEAD)"

docker build \
  --progress=plain \
  --build-arg "VERGEN_GIT_SHA=$GIT_SHA" \
  --build-arg "VERGEN_GIT_COMMIT_TIMESTAMP=$GIT_COMMIT_TIMESTAMP" \
  -t "$IMAGE" \
  -f "$REPO_ROOT/self-hosted/docker-build/Dockerfile.backend" \
  "$REPO_ROOT"
