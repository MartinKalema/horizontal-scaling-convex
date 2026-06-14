# Benchmarking

Check out our open-source benchmarking tool,
[LoadGenerator](../../crates/load_generator/README.md), for more information on
how to benchmark and load test your Convex instance.

## Clustered Commit Hot Path

For the experimental partitioned Docker cluster, use the dedicated commit-path
benchmark:

```sh
cd self-hosted/docker
OPS=200 CONCURRENCY=8 ./benchmark-commit-hot-path.sh
```

The script deploys small benchmark mutations, then reports throughput plus
p50/p95/p99 mutation latency for local partition writes and cross-partition
2PC writes. It is intended for before/after comparisons while working on the
cluster commit path; it is not a correctness substitute for
`self-hosted/docker/test.sh`.
