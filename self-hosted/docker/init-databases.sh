#!/bin/bash
set -e

# Dynamically create PostgreSQL databases for all Convex nodes.
# Reads CONVEX_INSTANCES env var: comma-separated list of instance names.
# Each instance name is converted to a database name by replacing - with _.
#
# Example:
#   CONVEX_INSTANCES=convex-primary,convex-replica,convex-replica2
#   Creates: convex_primary, convex_replica, convex_replica2
#
#   CONVEX_INSTANCES=convex-node-a,convex-node-b,convex-node-c,convex-node-d
#   Creates: convex_node_a, convex_node_b, convex_node_c, convex_node_d

if [ -z "$CONVEX_INSTANCES" ]; then
    echo "CONVEX_INSTANCES not set, skipping dynamic database creation"
    exit 0
fi

IFS=',' read -ra INSTANCES <<< "$CONVEX_INSTANCES"
for instance in "${INSTANCES[@]}"; do
    instance=$(echo "$instance" | xargs) # trim whitespace
    db_name=$(echo "$instance" | tr '-' '_')
    echo "Creating database: $db_name (for instance: $instance)"
    psql -v ON_ERROR_STOP=0 --username "$POSTGRES_USER" <<-EOSQL
        CREATE DATABASE "$db_name";
EOSQL
done

echo "Created ${#INSTANCES[@]} databases"
