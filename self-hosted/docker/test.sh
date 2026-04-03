#!/usr/bin/env bash
#
# Convex Cluster Integration Tests
#
# One test script, two suites — same pattern as CockroachDB roachtest.
# Raft consensus tests run last (they kill nodes).
#
# Usage:
#   ./test.sh              # Run all tests (scaling + failover)
#   ./test.sh scaling      # Write scaling only (77 tests)
#   ./test.sh failover     # Raft failover only (10 tests)
#
# Prerequisites:
#   docker compose --profile cluster up

set -euo pipefail

SUITE="${1:-all}"

case "$SUITE" in
    all|scaling|failover) ;;
    *)
        echo "Usage: $0 [all|scaling|failover]"
        echo "  all       Run all tests (default)"
        echo "  scaling   Write scaling tests only (77 tests)"
        echo "  failover  Raft failover tests only (10 tests)"
        exit 1
        ;;
esac

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"

echo ""
echo "============================================================"
echo "  Convex Cluster Integration Tests"
echo "  Suite: $SUITE"
echo "============================================================"
echo ""

TOTAL_PASSED=0
TOTAL_FAILED=0

if [ "$SUITE" = "all" ] || [ "$SUITE" = "scaling" ]; then
    echo "── Running Write Scaling Suite ──────────────────────────────"
    echo ""
    if bash "$SCRIPT_DIR/test-write-scaling.sh"; then
        echo ""
        echo "  Write scaling suite: PASSED"
    else
        TOTAL_FAILED=$((TOTAL_FAILED + 1))
        echo ""
        echo "  Write scaling suite: FAILED"
    fi
    echo ""
fi

if [ "$SUITE" = "all" ] || [ "$SUITE" = "failover" ]; then
    echo "── Running Raft Failover Suite ─────────────────────────────"
    echo ""
    if bash "$SCRIPT_DIR/test-raft-failover.sh"; then
        echo ""
        echo "  Raft failover suite: PASSED"
    else
        TOTAL_FAILED=$((TOTAL_FAILED + 1))
        echo ""
        echo "  Raft failover suite: FAILED"
    fi
    echo ""
fi

echo "============================================================"
if [ "$TOTAL_FAILED" -eq 0 ]; then
    echo "  ALL SUITES PASSED"
else
    echo "  $TOTAL_FAILED SUITE(S) FAILED"
fi
echo "============================================================"
echo ""

exit "$TOTAL_FAILED"
