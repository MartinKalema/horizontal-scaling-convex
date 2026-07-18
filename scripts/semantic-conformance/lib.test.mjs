import assert from "node:assert/strict";
import { spawnSync } from "node:child_process";
import { readFileSync } from "node:fs";
import test from "node:test";
import { fileURLToPath } from "node:url";

import {
  INVARIANT_IDS,
  releaseBlockers,
  selectProofs,
  summarizeDimensions,
  validateMatrix,
  validatePullRequestBody,
} from "./lib.mjs";

const matrix = JSON.parse(
  readFileSync(new URL("./matrix.json", import.meta.url), "utf8"),
);

test("checked-in semantic conformance matrix is valid", () => {
  assert.equal(validateMatrix(matrix), matrix);
  assert.deepEqual(matrix.invariants, INVARIANT_IDS);
});

test("matrix rejects unknown proof references", () => {
  const invalid = structuredClone(matrix);
  invalid.dimensions[0].proofs.push("missing-proof");
  assert.throws(() => validateMatrix(invalid), /unknown proof missing-proof/);
});

test("matrix rejects duplicate dimension IDs", () => {
  const invalid = structuredClone(matrix);
  invalid.dimensions[1].id = invalid.dimensions[0].id;
  assert.throws(
    () => validateMatrix(invalid),
    /dimension IDs contain duplicates/,
  );
});

test("matrix requires an owner issue and executable evidence", () => {
  const invalid = structuredClone(matrix);
  invalid.dimensions[0].ownerIssues = [];
  invalid.dimensions[0].proofs = [];
  assert.throws(
    () => validateMatrix(invalid),
    /ownerIssues must contain at least one issue number/,
  );
  assert.throws(() => validateMatrix(invalid), /proofs must not be empty/);
});

test("release blockers report every required unproven dimension", () => {
  const blockers = releaseBlockers(matrix);
  assert.equal(
    blockers.length,
    matrix.dimensions.filter((dimension) => dimension.status !== "proven")
      .length,
  );
  assert.ok(blockers.some((blocker) => blocker.status === "blocked"));
  assert.ok(blockers.some((blocker) => blocker.status === "partial"));
});

test("release eligibility requires every dimension to be proven", () => {
  const proven = structuredClone(matrix);
  for (const dimension of proven.dimensions) {
    dimension.status = "proven";
    delete dimension.gap;
  }
  assert.deepEqual(releaseBlockers(proven), []);
});

test("proven dimensions cannot retain known gaps", () => {
  const invalid = structuredClone(matrix);
  invalid.dimensions[0].status = "proven";
  assert.throws(
    () => validateMatrix(invalid),
    /gap must be removed when status is proven/,
  );
});

test("proof selection expands monotonically from PR to release", () => {
  const prProofs = selectProofs(matrix, "pr");
  const nightlyProofs = selectProofs(matrix, "nightly");
  const releaseProofs = selectProofs(matrix, "release");

  assert.ok(prProofs.length > 0);
  assert.ok(nightlyProofs.length > prProofs.length);
  assert.ok(releaseProofs.length >= nightlyProofs.length);
  assert.ok(prProofs.every((proof) => proof.tier === "pr"));
  assert.ok(nightlyProofs.some((proof) => proof.tier === "nightly"));
});

test("dimension summary accounts for every required semantic dimension", () => {
  const summary = summarizeDimensions(matrix);
  assert.equal(
    summary.reduce((total, entry) => total + entry.count, 0),
    matrix.dimensions.length,
  );
});

test("strict release CLI rejects the checked-in unproven matrix", () => {
  const result = spawnSync(
    process.execPath,
    [
      fileURLToPath(new URL("./run.mjs", import.meta.url)),
      "--mode",
      "release",
      "--dry-run",
    ],
    { encoding: "utf8" },
  );
  assert.equal(result.status, 1);
  assert.match(
    result.stderr,
    /Release blocked by unproven semantic dimensions/,
  );
});

test("correctness-sensitive pull requests name invariants and executable proof", () => {
  const body = `
Affected invariants and authority:

INV-03 and INV-05. The partition Raft log remains authoritative.

### Visibility And Time

Dangerous interleaving tested:

test_prepare_survives_leader_failover forces the crash after prepare quorum.

### Transactions And Reactivity
`;
  assert.deepEqual(validatePullRequestBody(body), {
    invariantIds: ["INV-03", "INV-05"],
    notApplicable: false,
  });
});

test("pull requests can declare semantic review not applicable with a reason", () => {
  const body = `
Affected invariants and authority:

Not applicable: this change only corrects spelling in contributor documentation.

### Visibility And Time
`;
  assert.deepEqual(validatePullRequestBody(body), {
    invariantIds: [],
    notApplicable: true,
  });
});

test("pull request contract rejects unknown invariants and missing proof", () => {
  const unknown = `
Affected invariants and authority:

INV-99. The partition Raft log remains authoritative.

### Visibility And Time
`;
  assert.throws(
    () => validatePullRequestBody(unknown),
    /unknown invariants: INV-99/,
  );

  const missingProof = `
Affected invariants and authority:

INV-03. The partition Raft log remains authoritative.

### Visibility And Time

Dangerous interleaving tested:

<!-- Add the proof. -->

### Transactions And Reactivity
`;
  assert.throws(
    () => validatePullRequestBody(missingProof),
    /must complete 'Dangerous interleaving tested:'/,
  );
});
