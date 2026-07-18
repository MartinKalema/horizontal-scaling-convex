#!/usr/bin/env node

import { spawn, spawnSync } from "node:child_process";
import { createHash, randomBytes } from "node:crypto";
import {
  createWriteStream,
  existsSync,
  mkdirSync,
  readFileSync,
  writeFileSync,
} from "node:fs";
import os from "node:os";
import path from "node:path";
import process from "node:process";
import { fileURLToPath } from "node:url";
import { finished } from "node:stream/promises";

import {
  releaseBlockers,
  selectProofs,
  summarizeDimensions,
  validateMatrix,
} from "./lib.mjs";

const scriptDir = path.dirname(fileURLToPath(import.meta.url));
const repoRoot = path.resolve(scriptDir, "../..");

function usage() {
  console.log(`Usage: node scripts/semantic-conformance/run.mjs [options]

Options:
  --mode <validate|pr|nightly|release>  Validation depth (default: validate)
  --matrix <path>                      Alternate matrix JSON
  --artifacts-dir <path>               Artifact root (default: artifacts/semantic-conformance)
  --seed <value>                       Reproducible run seed
  --dry-run                            Validate and list commands without executing them
  --help                               Show this help
`);
}

function parseArgs(argv) {
  const args = {
    mode: "validate",
    matrixPath: path.join(scriptDir, "matrix.json"),
    artifactsDir: path.join(repoRoot, "artifacts/semantic-conformance"),
    seed:
      process.env.SEMANTIC_CONFORMANCE_SEED || randomBytes(8).toString("hex"),
    dryRun: false,
  };

  for (let index = 0; index < argv.length; index += 1) {
    const arg = argv[index];
    switch (arg) {
      case "--mode":
        args.mode = argv[++index];
        break;
      case "--matrix":
        args.matrixPath = path.resolve(repoRoot, argv[++index]);
        break;
      case "--artifacts-dir":
        args.artifactsDir = path.resolve(repoRoot, argv[++index]);
        break;
      case "--seed":
        args.seed = argv[++index];
        break;
      case "--dry-run":
        args.dryRun = true;
        break;
      case "--help":
        usage();
        process.exit(0);
        break;
      default:
        throw new Error(`Unknown argument: ${arg}`);
    }
  }

  for (const [name, value] of Object.entries({
    mode: args.mode,
    matrix: args.matrixPath,
    artifacts: args.artifactsDir,
    seed: args.seed,
  })) {
    if (typeof value !== "string" || value.length === 0) {
      throw new Error(`${name} requires a non-empty value`);
    }
  }
  return args;
}

function commandOutput(command, commandArgs) {
  const result = spawnSync(command, commandArgs, {
    cwd: repoRoot,
    encoding: "utf8",
  });
  return result.status === 0 ? result.stdout.trim() : null;
}

function defaultProtocPath() {
  const binaryByPlatform = {
    "darwin-arm64": "protoc-macos-universal",
    "darwin-x64": "protoc-macos-universal",
    "linux-arm64": "protoc-linux-aarch64",
    "linux-x64": "protoc-linux-x86_64",
    "win32-x64": "protoc-windows-x86_64",
  };
  const binary = binaryByPlatform[`${process.platform}-${process.arch}`];
  if (!binary) {
    return null;
  }
  const candidate = path.join(repoRoot, "crates/pb_build/protoc", binary);
  return existsSync(candidate) ? candidate : null;
}

function expand(value, replacements) {
  return value.replace(
    /\$\{([A-Z_]+)\}/g,
    (match, key) => replacements[key] ?? match,
  );
}

function missingRequirement(requirement) {
  if (requirement === "cluster") {
    return process.env.SEMANTIC_CLUSTER_READY === "1"
      ? null
      : "SEMANTIC_CLUSTER_READY=1 and a healthy locally built cluster are required";
  }
  return `unknown proof requirement: ${requirement}`;
}

async function runProof(proof, context) {
  const proofDir = path.join(context.runDir, proof.id);
  mkdirSync(proofDir, { recursive: true });
  const logPath = path.join(proofDir, "output.log");
  const resultPath = path.join(proofDir, "result.json");
  const startedAt = new Date();

  for (const requirement of proof.requires ?? []) {
    const missing = missingRequirement(requirement);
    if (missing) {
      const result = {
        id: proof.id,
        status: "failed",
        reason: missing,
        startedAt: startedAt.toISOString(),
        finishedAt: new Date().toISOString(),
      };
      writeFileSync(resultPath, `${JSON.stringify(result, null, 2)}\n`);
      console.error(`\n[FAIL] ${proof.id}: ${missing}`);
      return result;
    }
  }

  const replacements = {
    ARTIFACT_DIR: proofDir,
    REPO_ROOT: repoRoot,
    SEED: context.seed,
  };
  const [command, ...commandArgs] = proof.command.map((entry) =>
    expand(entry, replacements),
  );
  const proofEnv = Object.fromEntries(
    Object.entries(proof.env ?? {}).map(([key, value]) => [
      key,
      expand(value, replacements),
    ]),
  );
  const env = {
    ...process.env,
    ...proofEnv,
    SEMANTIC_CONFORMANCE_SEED: context.seed,
    CONVEX_TEST_SEED: context.seed,
  };
  if (!env.PROTOC) {
    const protoc = defaultProtocPath();
    if (protoc) {
      env.PROTOC = protoc;
    }
  }

  console.log(`\n[RUN] ${proof.id}: ${[command, ...commandArgs].join(" ")}`);
  const log = createWriteStream(logPath, { flags: "w" });
  log.write(
    `proof=${proof.id}\nseed=${context.seed}\ncommand=${[command, ...commandArgs].join(" ")}\n\n`,
  );

  const child = spawn(command, commandArgs, {
    cwd: repoRoot,
    detached: process.platform !== "win32",
    env,
    stdio: ["ignore", "pipe", "pipe"],
  });

  child.stdout.on("data", (chunk) => {
    process.stdout.write(chunk);
    log.write(chunk);
  });
  child.stderr.on("data", (chunk) => {
    process.stderr.write(chunk);
    log.write(chunk);
  });

  const killChildTree = (signal) => {
    if (child.pid === undefined) {
      return;
    }
    try {
      if (process.platform === "win32") {
        child.kill(signal);
      } else {
        process.kill(-child.pid, signal);
      }
    } catch (error) {
      if (error.code !== "ESRCH") {
        throw error;
      }
    }
  };

  let timedOut = false;
  let forceKill;
  const timeout = setTimeout(() => {
    timedOut = true;
    killChildTree("SIGTERM");
    forceKill = setTimeout(() => killChildTree("SIGKILL"), 5000);
    forceKill.unref();
  }, proof.timeoutSeconds * 1000);

  const exit = await new Promise((resolve) => {
    child.once("error", (error) =>
      resolve({ code: null, signal: null, error }),
    );
    child.once("close", (code, signal) =>
      resolve({ code, signal, error: null }),
    );
  });
  clearTimeout(timeout);
  clearTimeout(forceKill);
  log.end();
  await finished(log);

  const finishedAt = new Date();
  const passed = !timedOut && !exit.error && exit.code === 0;
  const result = {
    id: proof.id,
    title: proof.title,
    status: passed ? "passed" : "failed",
    command: [command, ...commandArgs],
    seed: context.seed,
    exitCode: exit.code,
    signal: exit.signal,
    timedOut,
    error: exit.error?.message ?? null,
    startedAt: startedAt.toISOString(),
    finishedAt: finishedAt.toISOString(),
    durationMs: finishedAt.getTime() - startedAt.getTime(),
    log: path.relative(context.runDir, logPath),
  };
  writeFileSync(resultPath, `${JSON.stringify(result, null, 2)}\n`);
  console.log(`\n[${passed ? "PASS" : "FAIL"}] ${proof.id}`);
  return result;
}

async function main() {
  const args = parseArgs(process.argv.slice(2));
  const rawMatrix = readFileSync(args.matrixPath, "utf8");
  const matrix = validateMatrix(JSON.parse(rawMatrix));
  const summary = summarizeDimensions(matrix);

  console.log(
    `Semantic conformance matrix: ${matrix.dimensions.length} dimensions`,
  );
  for (const entry of summary) {
    console.log(`  ${entry.status}: ${entry.count}`);
  }

  if (args.mode === "release") {
    const blockers = releaseBlockers(matrix);
    if (blockers.length > 0) {
      console.error("\nRelease blocked by unproven semantic dimensions:");
      for (const blocker of blockers) {
        console.error(
          `  - ${blocker.id} [${blocker.status}] issues ${blocker.ownerIssues
            .map((issue) => `#${issue}`)
            .join(", ")}: ${blocker.gap}`,
        );
      }
      process.exitCode = 1;
      return;
    }
  }

  const proofs = selectProofs(matrix, args.mode);
  if (args.mode === "validate") {
    console.log("Matrix contract: valid");
    return;
  }

  console.log(`Mode ${args.mode} selected ${proofs.length} proof command(s).`);
  for (const proof of proofs) {
    console.log(`  - [${proof.tier}] ${proof.id}: ${proof.command.join(" ")}`);
  }
  if (args.dryRun) {
    console.log("Dry run complete; no proof commands executed.");
    return;
  }

  const timestamp = new Date().toISOString().replace(/[:.]/g, "-");
  const runDir = path.join(
    args.artifactsDir,
    `${args.mode}-${timestamp}-${args.seed}`,
  );
  mkdirSync(runDir, { recursive: true });

  const metadata = {
    schemaVersion: 1,
    mode: args.mode,
    seed: args.seed,
    gitSha: commandOutput("git", ["rev-parse", "HEAD"]),
    gitStatus: commandOutput("git", ["status", "--short"]),
    matrixPath: path.relative(repoRoot, args.matrixPath),
    matrixSha256: createHash("sha256").update(rawMatrix).digest("hex"),
    platform: process.platform,
    arch: process.arch,
    nodeVersion: process.version,
    hostname: os.hostname(),
    startedAt: new Date().toISOString(),
    proofs: proofs.map((proof) => proof.id),
  };
  writeFileSync(
    path.join(runDir, "run.json"),
    `${JSON.stringify(metadata, null, 2)}\n`,
  );

  const results = [];
  for (const proof of proofs) {
    results.push(await runProof(proof, { runDir, seed: args.seed }));
  }

  const failed = results.filter((result) => result.status !== "passed");
  const summaryResult = {
    ...metadata,
    finishedAt: new Date().toISOString(),
    passed: failed.length === 0,
    results,
  };
  writeFileSync(
    path.join(runDir, "summary.json"),
    `${JSON.stringify(summaryResult, null, 2)}\n`,
  );
  console.log(`\nArtifacts: ${path.relative(repoRoot, runDir)}`);
  if (failed.length > 0) {
    console.error(
      `Semantic conformance failed: ${failed.length} proof(s) failed.`,
    );
    process.exitCode = 1;
  } else {
    console.log(
      `Semantic conformance passed: ${results.length} proof(s) passed.`,
    );
  }
}

main().catch((error) => {
  console.error(error.stack ?? error.message ?? String(error));
  process.exitCode = 1;
});
