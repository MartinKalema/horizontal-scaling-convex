#!/usr/bin/env node

import process from "node:process";

import { validatePullRequestBody } from "./lib.mjs";

try {
  const result = validatePullRequestBody(process.env.PR_BODY);
  if (result.notApplicable) {
    console.log(
      "Pull request semantic declaration: not applicable with reason",
    );
  } else {
    console.log(
      `Pull request semantic declaration: ${result.invariantIds.join(", ")}`,
    );
  }
} catch (error) {
  console.error(error.message ?? String(error));
  process.exitCode = 1;
}
