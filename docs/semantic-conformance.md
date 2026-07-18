# Semantic Conformance Gate

- **Tracks:** GitHub issue `#288`

**Normative contract:**
[Distributed Correctness Model](distributed-correctness-model.md)

The conformance gate answers a different question from a normal test suite:

> Can this revision be released as a horizontally scaled Convex without
> weakening observable Convex semantics?

A protocol test can pass while that answer is still no. The checked-in matrix
therefore records both executable evidence and the remaining semantic gap.

## Current Release Status

The strict release gate is intentionally **blocked**.

The matrix currently contains:

| Status | Count | Meaning |
| --- | ---: | --- |
| Proven | 0 | Complete executable proof for the required semantic dimension |
| Partial | 6 | Useful tests exist, but a required failure surface or architecture proof is missing |
| Blocked | 6 | A required protocol or feature is not implemented yet |

This is not a test failure or a reason to weaken the gate. It is the honest
state of the project. The owner issues printed by release mode are the work
required to make progress.

## Machine-Readable Matrix

The source of truth is
[`scripts/semantic-conformance/matrix.json`](../scripts/semantic-conformance/matrix.json).

Each semantic dimension contains:

- a stable ID and title;
- the `INV-01` through `INV-16` contracts it protects;
- one or more owning GitHub issues;
- an explicit `proven`, `partial`, or `blocked` status;
- executable proof IDs;
- a plain-language statement of current evidence;
- the exact gap preventing release when it is not proven.

Each proof contains an argument-vector command, tier, kind, timeout, and any
runtime prerequisites. Commands are not shell strings, which avoids accidental
shell expansion and makes the recorded command reproducible.

The validator rejects:

- unknown or duplicate invariants, dimensions, and proofs;
- semantic dimensions without an owner issue;
- dimensions without executable evidence;
- proofs that are not attached to a semantic dimension;
- `partial` or `blocked` entries without a documented gap;
- an attempt to run release mode while any required dimension is unproven.

Pull-request CI also validates the distributed-correctness section of the PR
description. A correctness-sensitive change must name valid invariant IDs and
the executable proof that forces its dangerous interleaving. A change outside
the semantic surface must say `Not applicable:` and provide a concrete reason;
leaving the template blank does not pass.

## Modes

### Validate

Checks only the matrix contract. It does not run proof commands.

```sh
node scripts/semantic-conformance/run.mjs --mode validate
```

### Pull Request

Runs a bounded deterministic set on every pull request and push to `dev`:

- conformance runner contract tests;
- database write-scaling tests;
- seeded Raft failover/nemesis tests;
- local-backend public API and authority tests.

```sh
node scripts/semantic-conformance/run.mjs --mode pr
```

Use `--dry-run` to inspect the selected commands without executing them.

### Nightly

Includes all PR proofs plus:

- the seeded Elle history model;
- the real six-node Docker write-scaling and Raft failover harness.

The cluster must be built locally and its exact image ID verified before the
runner accepts cluster proofs:

```sh
scripts/semantic-conformance/prepare-cluster.sh \
  artifacts/semantic-conformance/cluster

SEMANTIC_CLUSTER_READY=1 \
  node scripts/semantic-conformance/run.mjs --mode nightly
```

### Release

Release mode has two gates:

1. Every required dimension must be `proven`.
2. Every PR, nightly, and release-tier proof must pass.

```sh
node scripts/semantic-conformance/run.mjs --mode release --dry-run
```

That command currently exits nonzero and prints all blockers. Removing a
dimension, marking it optional, or skipping its test is not an allowed way to
make a release pass.

## CI Workflows

| Workflow | Trigger | Runner | Purpose |
| --- | --- | --- | --- |
| `semantic-conformance-pr.yml` | Pull requests and pushes to `dev` | GitHub-hosted Linux | Validate the PR semantic declaration and run bounded deterministic proof on every change |
| `semantic-conformance-nightly.yml` | Daily schedule or manual dispatch | `self-hosted`, `linux`, `x64`, `convex-cluster` | Build a local image, verify all six containers, run Elle and the full cluster harness |
| `semantic-conformance-release.yml` | Reusable workflow or manual dispatch | GitHub-hosted eligibility plus capacity-labeled cluster runner | Refuse release while unproven, then run the full proof set |

The nightly/release runner label is deliberate. A clean backend image plus six
backend containers needs substantially more disk and memory than a standard
lightweight CI job. The runner must provide Docker with Compose, `rustup`,
`cmake`, `libclang`, at least 16 GB of RAM, and at least 80 GB of free disk.
Infrastructure automation for that runner belongs with issue `#71`.

Any release workflow must call `semantic-conformance-release.yml` and require
it to succeed before publishing artifacts or a GitHub release.

## Replay Artifacts

Every executed run writes an ignored local directory under
`artifacts/semantic-conformance/`. CI uploads it even when a proof fails.

The run metadata includes:

- conformance mode and seed;
- git SHA and dirty status;
- SHA-256 of the exact matrix;
- platform, architecture, Node version, and host;
- selected proof IDs and commands;
- start/end times, timeout, signal, and exit code;
- complete stdout/stderr per proof.

Cluster workflows additionally retain:

- the locally built backend image ID and revision;
- service/container/image/health mapping for all six backend nodes;
- Compose process state;
- per-node network membership;
- timestamped backend logs, which carry terms, epochs, commit timestamps, and
  protocol diagnostics emitted by the implementation;
- the full harness event stream.

Replay a run with its recorded seed:

```sh
SEMANTIC_CONFORMANCE_SEED=<seed> \
  node scripts/semantic-conformance/run.mjs --mode pr
```

Tests with their own deterministic seed must print it to stdout so it is also
captured in the proof log.

## Updating The Matrix

When a correctness-sensitive PR changes behavior:

1. Name the affected invariant IDs in the PR.
2. Add or update a proof that forces the dangerous interleaving.
3. Attach the proof ID to every affected semantic dimension.
4. Update `currentEvidence` and `gap` honestly.
5. Change a dimension to `proven` only when all owner issues required by that
   semantic contract are complete and its release-tier evidence passes.
6. Preserve the failing seed/history as a regression test when fixing a found
   anomaly.

Do not count eventual convergence as proof of serializability, atomic
visibility, read-your-writes, or reactive completeness. Final equality can hide
a torn intermediate read, a missed invalidation, or a transaction that had no
legal serialization order.

## Adding A New Semantic Dimension

A new dimension is required when a developer-visible Convex behavior is not
covered by the existing twelve dimensions. It must be `required: true`, name at
least one owner issue and invariant, and attach at least one executable proof.
If the implementation does not exist, mark it `blocked`; do not omit it from
the release contract.
