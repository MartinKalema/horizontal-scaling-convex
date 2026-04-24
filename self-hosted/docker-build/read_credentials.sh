#!/usr/bin/env bash

DATA_DIR=${DATA_DIR:-/convex/data}
CREDENTIALS_DIR=${CREDENTIALS_DIR:-"$DATA_DIR/credentials"}
SHARED_INSTANCE_SECRET_PATH=${SHARED_INSTANCE_SECRET_PATH:-}

set -e
mkdir -p "$CREDENTIALS_DIR"

# Set INSTANCE_SECRET by checking in order:
# 1. Use existing INSTANCE_SECRET env var if set
# 2. Read from SHARED_INSTANCE_SECRET_PATH if configured and present
# 3. Read from CREDENTIALS_DIR/instance_secret if file exists
# 4. Generate new random secret if none exist
# Finally, save the secret to disk for persistence
if [[ -n "$SHARED_INSTANCE_SECRET_PATH" ]]; then
  mkdir -p "$(dirname "$SHARED_INSTANCE_SECRET_PATH")"
fi

SHARED_INSTANCE_SECRET=""
if [[ -n "$SHARED_INSTANCE_SECRET_PATH" ]]; then
  SHARED_INSTANCE_SECRET=$(cat "$SHARED_INSTANCE_SECRET_PATH" 2>/dev/null || true)
fi

export INSTANCE_SECRET=${INSTANCE_SECRET:-${SHARED_INSTANCE_SECRET:-$(cat "$CREDENTIALS_DIR/instance_secret" 2>/dev/null || openssl rand -hex 32)}}
echo "$INSTANCE_SECRET" > "$CREDENTIALS_DIR/instance_secret"
if [[ -n "$SHARED_INSTANCE_SECRET_PATH" ]]; then
  echo "$INSTANCE_SECRET" > "$SHARED_INSTANCE_SECRET_PATH"
fi

# Set INSTANCE_NAME by checking in order:
# 1. Use existing INSTANCE_NAME env var if set
# 2. Read from CREDENTIALS_DIR/instance_name if file exists
# 3. Use default name "convex-self-hosted" if neither exists
# Finally, save the name to disk for persistence
export INSTANCE_NAME=${INSTANCE_NAME:-$(cat "$CREDENTIALS_DIR/instance_name" 2>/dev/null || echo "convex-self-hosted")}
echo "$INSTANCE_NAME" > "$CREDENTIALS_DIR/instance_name"
