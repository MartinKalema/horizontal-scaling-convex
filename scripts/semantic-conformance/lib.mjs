export const INVARIANT_IDS = Object.freeze(
  Array.from(
    { length: 16 },
    (_, index) => `INV-${String(index + 1).padStart(2, "0")}`,
  ),
);

export const DIMENSION_STATUSES = Object.freeze([
  "proven",
  "partial",
  "blocked",
]);
export const PROOF_TIERS = Object.freeze(["pr", "nightly", "release"]);
export const PROOF_KINDS = Object.freeze([
  "contract",
  "unit",
  "deterministic-fault",
  "history-checker",
  "cluster",
]);

const INVARIANT_PATTERN = /\bINV-\d{2}\b/g;

function isNonEmptyString(value) {
  return typeof value === "string" && value.trim().length > 0;
}

function duplicateValues(values) {
  const seen = new Set();
  const duplicates = new Set();
  for (const value of values) {
    if (seen.has(value)) {
      duplicates.add(value);
    }
    seen.add(value);
  }
  return [...duplicates].sort();
}

function validateStringArray(errors, value, path, { nonEmpty = true } = {}) {
  if (!Array.isArray(value)) {
    errors.push(`${path} must be an array`);
    return;
  }
  if (nonEmpty && value.length === 0) {
    errors.push(`${path} must not be empty`);
  }
  value.forEach((entry, index) => {
    if (!isNonEmptyString(entry)) {
      errors.push(`${path}[${index}] must be a non-empty string`);
    }
  });
}

function sectionContent(body, label) {
  const start = body.indexOf(label);
  if (start === -1) {
    return null;
  }
  const remainder = body.slice(start + label.length);
  const nextHeading = remainder.search(/^#{2,3}\s/m);
  const section =
    nextHeading === -1 ? remainder : remainder.slice(0, nextHeading);
  return section.replace(/<!--[\s\S]*?-->/g, "").trim();
}

function nonApplicableReason(content) {
  const match = content.match(/^not applicable\s*:\s*(.+)$/is);
  return match?.[1].trim() ?? null;
}

export function validatePullRequestBody(body) {
  if (!isNonEmptyString(body)) {
    throw new Error("Pull request body must not be empty");
  }

  const contract = sectionContent(body, "Affected invariants and authority:");
  if (!isNonEmptyString(contract)) {
    throw new Error(
      "Pull request body must complete 'Affected invariants and authority:'",
    );
  }

  const notApplicable = nonApplicableReason(contract);
  if (notApplicable !== null) {
    if (notApplicable.length < 20) {
      throw new Error(
        "A not-applicable invariant declaration needs a concrete reason",
      );
    }
    return { invariantIds: [], notApplicable: true };
  }

  const referencedIds = [...new Set(contract.match(INVARIANT_PATTERN) ?? [])];
  const unknownIds = referencedIds.filter((id) => !INVARIANT_IDS.includes(id));
  if (unknownIds.length > 0) {
    throw new Error(
      `Pull request body references unknown invariants: ${unknownIds.join(", ")}`,
    );
  }
  if (referencedIds.length === 0) {
    throw new Error(
      "Pull request body must name at least one INV-01 through INV-16 invariant",
    );
  }

  const interleaving = sectionContent(body, "Dangerous interleaving tested:");
  if (!isNonEmptyString(interleaving)) {
    throw new Error(
      "Pull request body must complete 'Dangerous interleaving tested:'",
    );
  }
  if (nonApplicableReason(interleaving) !== null) {
    throw new Error(
      "A correctness-sensitive pull request must name its executable proof, not mark the dangerous interleaving as not applicable",
    );
  }

  return { invariantIds: referencedIds, notApplicable: false };
}

export function validateMatrix(matrix) {
  const errors = [];

  if (!matrix || typeof matrix !== "object" || Array.isArray(matrix)) {
    throw new Error("Semantic conformance matrix must be a JSON object");
  }
  if (matrix.schemaVersion !== 1) {
    errors.push("schemaVersion must be 1");
  }

  validateStringArray(errors, matrix.invariants, "invariants");
  if (Array.isArray(matrix.invariants)) {
    const actual = [...matrix.invariants].sort();
    const expected = [...INVARIANT_IDS].sort();
    if (JSON.stringify(actual) !== JSON.stringify(expected)) {
      errors.push(`invariants must define exactly ${INVARIANT_IDS.join(", ")}`);
    }
    const duplicates = duplicateValues(matrix.invariants);
    if (duplicates.length > 0) {
      errors.push(`invariants contain duplicates: ${duplicates.join(", ")}`);
    }
  }

  if (!Array.isArray(matrix.proofs) || matrix.proofs.length === 0) {
    errors.push("proofs must be a non-empty array");
  }
  if (!Array.isArray(matrix.dimensions) || matrix.dimensions.length === 0) {
    errors.push("dimensions must be a non-empty array");
  }

  const proofIds = new Set();
  const proofsById = new Map();
  if (Array.isArray(matrix.proofs)) {
    const duplicateProofIds = duplicateValues(
      matrix.proofs.map((proof) => proof?.id),
    );
    if (duplicateProofIds.length > 0) {
      errors.push(
        `proof IDs contain duplicates: ${duplicateProofIds.join(", ")}`,
      );
    }

    matrix.proofs.forEach((proof, index) => {
      const path = `proofs[${index}]`;
      if (!proof || typeof proof !== "object" || Array.isArray(proof)) {
        errors.push(`${path} must be an object`);
        return;
      }
      if (!isNonEmptyString(proof.id)) {
        errors.push(`${path}.id must be a non-empty string`);
      } else {
        proofIds.add(proof.id);
        proofsById.set(proof.id, proof);
      }
      if (!isNonEmptyString(proof.title)) {
        errors.push(`${path}.title must be a non-empty string`);
      }
      if (!PROOF_TIERS.includes(proof.tier)) {
        errors.push(`${path}.tier must be one of ${PROOF_TIERS.join(", ")}`);
      }
      if (!PROOF_KINDS.includes(proof.kind)) {
        errors.push(`${path}.kind must be one of ${PROOF_KINDS.join(", ")}`);
      }
      validateStringArray(errors, proof.command, `${path}.command`);
      if (
        !Number.isInteger(proof.timeoutSeconds) ||
        proof.timeoutSeconds <= 0
      ) {
        errors.push(`${path}.timeoutSeconds must be a positive integer`);
      }
      if (proof.requires !== undefined) {
        validateStringArray(errors, proof.requires, `${path}.requires`, {
          nonEmpty: false,
        });
      }
      if (proof.env !== undefined) {
        if (
          !proof.env ||
          typeof proof.env !== "object" ||
          Array.isArray(proof.env)
        ) {
          errors.push(`${path}.env must be an object`);
        } else {
          for (const [key, value] of Object.entries(proof.env)) {
            if (!isNonEmptyString(key) || !isNonEmptyString(value)) {
              errors.push(
                `${path}.env entries must have non-empty string keys and values`,
              );
            }
          }
        }
      }
    });
  }

  const dimensionIds = [];
  const referencedProofIds = new Set();
  if (Array.isArray(matrix.dimensions)) {
    matrix.dimensions.forEach((dimension, index) => {
      const path = `dimensions[${index}]`;
      if (
        !dimension ||
        typeof dimension !== "object" ||
        Array.isArray(dimension)
      ) {
        errors.push(`${path} must be an object`);
        return;
      }
      if (!isNonEmptyString(dimension.id)) {
        errors.push(`${path}.id must be a non-empty string`);
      } else {
        dimensionIds.push(dimension.id);
      }
      if (!isNonEmptyString(dimension.title)) {
        errors.push(`${path}.title must be a non-empty string`);
      }
      if (dimension.required !== true) {
        errors.push(`${path}.required must be true for release semantics`);
      }
      if (!DIMENSION_STATUSES.includes(dimension.status)) {
        errors.push(
          `${path}.status must be one of ${DIMENSION_STATUSES.join(", ")}`,
        );
      }
      validateStringArray(errors, dimension.invariants, `${path}.invariants`);
      if (Array.isArray(dimension.invariants)) {
        for (const invariant of dimension.invariants) {
          if (!INVARIANT_IDS.includes(invariant)) {
            errors.push(
              `${path}.invariants references unknown invariant ${invariant}`,
            );
          }
        }
      }
      if (
        !Array.isArray(dimension.ownerIssues) ||
        dimension.ownerIssues.length === 0
      ) {
        errors.push(
          `${path}.ownerIssues must contain at least one issue number`,
        );
      } else {
        dimension.ownerIssues.forEach((issue, issueIndex) => {
          if (!Number.isInteger(issue) || issue <= 0) {
            errors.push(
              `${path}.ownerIssues[${issueIndex}] must be a positive integer`,
            );
          }
        });
      }
      validateStringArray(errors, dimension.proofs, `${path}.proofs`);
      if (Array.isArray(dimension.proofs)) {
        for (const proofId of dimension.proofs) {
          referencedProofIds.add(proofId);
          if (!proofIds.has(proofId)) {
            errors.push(`${path}.proofs references unknown proof ${proofId}`);
          }
        }
      }
      if (!isNonEmptyString(dimension.currentEvidence)) {
        errors.push(`${path}.currentEvidence must be a non-empty string`);
      }
      if (dimension.status !== "proven" && !isNonEmptyString(dimension.gap)) {
        errors.push(
          `${path}.gap is required while status is ${dimension.status}`,
        );
      }
      if (dimension.status === "proven" && isNonEmptyString(dimension.gap)) {
        errors.push(`${path}.gap must be removed when status is proven`);
      }
      if (
        dimension.status === "proven" &&
        Array.isArray(dimension.proofs) &&
        !dimension.proofs.some(
          (proofId) => proofsById.get(proofId)?.tier !== "pr",
        )
      ) {
        errors.push(
          `${path} must reference a nightly or release proof when proven`,
        );
      }
    });

    const duplicateDimensionIds = duplicateValues(dimensionIds);
    if (duplicateDimensionIds.length > 0) {
      errors.push(
        `dimension IDs contain duplicates: ${duplicateDimensionIds.join(", ")}`,
      );
    }
  }

  for (const proofId of proofIds) {
    if (!referencedProofIds.has(proofId)) {
      errors.push(
        `proof ${proofId} is not referenced by any semantic dimension`,
      );
    }
  }

  if (errors.length > 0) {
    throw new Error(
      `Invalid semantic conformance matrix:\n- ${errors.join("\n- ")}`,
    );
  }
  return matrix;
}

export function releaseBlockers(matrix) {
  validateMatrix(matrix);
  return matrix.dimensions
    .filter((dimension) => dimension.required && dimension.status !== "proven")
    .map((dimension) => ({
      id: dimension.id,
      status: dimension.status,
      ownerIssues: [...dimension.ownerIssues],
      gap: dimension.gap,
    }));
}

export function selectProofs(matrix, mode) {
  validateMatrix(matrix);
  const tiersByMode = {
    validate: [],
    pr: ["pr"],
    nightly: ["pr", "nightly"],
    release: ["pr", "nightly", "release"],
  };
  const tiers = tiersByMode[mode];
  if (!tiers) {
    throw new Error(`Unknown semantic conformance mode: ${mode}`);
  }
  return matrix.proofs.filter((proof) => tiers.includes(proof.tier));
}

export function summarizeDimensions(matrix) {
  validateMatrix(matrix);
  return DIMENSION_STATUSES.map((status) => ({
    status,
    count: matrix.dimensions.filter((dimension) => dimension.status === status)
      .length,
  }));
}
