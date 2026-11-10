#!/usr/bin/env node
// Dependency-free contract fixture check. It verifies the contract version in
// Rust/TypeScript, the sample JSON shapes and a few security/invariant rules.
// It does not replace a compiler; run `cargo test` and `pnpm typecheck` too.

import { readFileSync } from "node:fs";
import { dirname, join } from "node:path";
import { fileURLToPath } from "node:url";

const scriptDir = dirname(fileURLToPath(import.meta.url));
const root = join(scriptDir, "..");

function read(relativePath) {
  return readFileSync(join(root, relativePath), "utf8");
}

function readJson(relativePath) {
  return JSON.parse(read(relativePath));
}

function assert(condition, message) {
  if (!condition) {
    throw new Error(message);
  }
}

function isPlainObject(value) {
  return typeof value === "object" && value !== null && !Array.isArray(value);
}

const TASK_STATES = new Set([
  "queued",
  "running",
  "paused",
  "waiting_user",
  "succeeded",
  "failed",
  "cancelled",
]);

const ERROR_CATEGORIES = new Set([
  "invalid_request",
  "conflict",
  "not_found",
  "permission_denied",
  "unsupported",
  "network",
  "storage",
  "cancelled",
  "internal",
]);

const ERROR_CODE_PATTERN = /^[a-z][a-z0-9._-]{2,63}$/;

try {
  const domainSource = read("crates/domain/src/lib.rs");
  const tsSource = read("apps/desktop/src/contract.ts");

  const rustVersionMatch = domainSource.match(
    /pub const CONTRACT_VERSION:\s*u16\s*=\s*(\d+);/,
  );
  const tsVersionMatch = tsSource.match(
    /export const CONTRACT_VERSION\s*=\s*(\d+);/,
  );

  assert(rustVersionMatch, "Rust CONTRACT_VERSION was not found");
  assert(tsVersionMatch, "TypeScript CONTRACT_VERSION was not found");

  const rustVersion = Number(rustVersionMatch[1]);
  const tsVersion = Number(tsVersionMatch[1]);

  assert(
    rustVersion === tsVersion,
    `Rust contract version ${rustVersion} != TypeScript version ${tsVersion}`,
  );
  assert(
    rustVersion === 1,
    `this checker expects contract v1, found v${rustVersion}`,
  );

  const task = readJson("contracts/samples/task.json");
  assert(isPlainObject(task), "task sample must be an object");
  assert(typeof task.id === "string" && task.id.length > 0, "task.id");
  assert(
    typeof task.operationId === "string" && task.operationId.length > 0,
    "task.operationId",
  );
  assert(
    Number.isInteger(task.attemptGeneration) && task.attemptGeneration > 0,
    "task.attemptGeneration",
  );
  assert(TASK_STATES.has(task.state), "task.state");
  assert(typeof task.phase === "string" && task.phase.length > 0, "task.phase");
  assert(typeof task.cancelRequested === "boolean", "task.cancelRequested");
  if (task.progress !== undefined) {
    assert(isPlainObject(task.progress), "task.progress");
    assert(
      Number.isInteger(task.progress.current) && task.progress.current >= 0,
      "task.progress.current",
    );
    assert(
      task.progress.total === null ||
        (Number.isInteger(task.progress.total) && task.progress.total >= 0),
      "task.progress.total",
    );
    assert(
      task.progress.total === null ||
        task.progress.current <= task.progress.total,
      "task.progress.current must not exceed total",
    );
    assert(
      ["bytes", "items", "steps"].includes(task.progress.unit),
      "task.progress.unit",
    );
  }

  const event = readJson("contracts/samples/event.json");
  assert(typeof event.eventId === "string" && event.eventId.length > 0, "event.eventId");
  assert(event.schemaVersion === rustVersion, "event.schemaVersion");
  assert(
    typeof event.operationId === "string" && event.operationId.length > 0,
    "event.operationId",
  );
  assert(Number.isInteger(event.revision) && event.revision >= 0, "event.revision");
  assert(Number.isInteger(event.sequence) && event.sequence >= 0, "event.sequence");
  assert(
    Number.isInteger(event.attemptGeneration) && event.attemptGeneration > 0,
    "event.attemptGeneration",
  );
  assert(typeof event.kind === "string" && event.kind.length > 0, "event.kind");
  assert(isPlainObject(event.payload), "event.payload");

  const error = readJson("contracts/samples/error.json");
  assert(
    typeof error.code === "string" && ERROR_CODE_PATTERN.test(error.code),
    "error.code",
  );
  assert(ERROR_CATEGORIES.has(error.category), "error.category");
  assert(typeof error.retryable === "boolean", "error.retryable");

  const cancel = readJson("contracts/samples/cancel.json");
  assert(typeof cancel.taskId === "string" && cancel.taskId.length > 0, "cancel.taskId");
  assert(
    Number.isInteger(cancel.attemptGeneration) && cancel.attemptGeneration > 0,
    "cancel.attemptGeneration",
  );

  assert(
    Array.isArray(task.evidence) && task.evidence.every((item) => typeof item === "string"),
    "task.evidence",
  );

  const CAPABILITY_PATTERN = /^[a-z][a-z0-9._-]{0,63}$/;
  const AVAILABILITY_STATES = new Set([
    "available",
    "unavailable",
    "unknown",
    "in_progress",
  ]);

  const command = readJson("contracts/samples/command.json");
  assert(typeof command.requestId === "string" && command.requestId.length > 0, "command.requestId");
  assert(
    typeof command.operationId === "string" && command.operationId.length > 0,
    "command.operationId",
  );
  assert(isPlainObject(command.payload), "command.payload");
  assert(
    typeof command.payload.command === "string" &&
      CAPABILITY_PATTERN.test(command.payload.command),
    "command.payload.command",
  );
  assert(
    command.payload.action === undefined,
    "command.payload.action is not an accepted alias for payload.command",
  );

  const message = readJson("contracts/samples/user-message.json");
  assert(ERROR_CODE_PATTERN.test(message.code), "userMessage.code");
  assert(typeof message.codeKnown === "boolean", "userMessage.codeKnown");
  assert(
    [
      "platform",
      "account",
      "game",
      "network",
      "local_file",
      "content",
      "room",
      "server",
      "launch",
      "runtime",
    ].includes(message.category),
    "userMessage.category",
  );
  assert(typeof message.title === "string" && message.title.length > 0, "userMessage.title");
  assert(Array.isArray(message.actions), "userMessage.actions");
  for (const action of message.actions) {
    assert(typeof action.label === "string" && action.label.length > 0, "userMessage.actions[].label");
    assert(CAPABILITY_PATTERN.test(action.capability), "userMessage.actions[].capability");
    assert(["primary", "secondary"].includes(action.role), "userMessage.actions[].role");
    assert(
      action.kind === undefined || action.kind === "detail",
      "userMessage.actions[].kind",
    );
  }
  assert(Array.isArray(message.evidence), "userMessage.evidence");
  assert(typeof message.contentVersion === "string", "userMessage.contentVersion");

  const availability = readJson("contracts/samples/availability.json");
  assert(availability.schemaVersion === "availability.v1", "availability.schemaVersion");
  assert(
    availability.requiresRecheckAtExecution === true,
    "availability projection must be re-checked at execution time",
  );
  assert(Number.isInteger(availability.revision), "availability.revision");
  assert(Array.isArray(availability.actions), "availability.actions");
  for (const action of availability.actions) {
    assert(CAPABILITY_PATTERN.test(action.capability), "availability.actions[].capability");
    assert(AVAILABILITY_STATES.has(action.state), "availability.actions[].state");
    assert(typeof action.available === "boolean", "availability.actions[].available");
    assert(
      action.available === (action.state === "available"),
      "availability.actions[].available must agree with state",
    );
    assert(
      action.nextStep === undefined || CAPABILITY_PATTERN.test(action.nextStep),
      "availability.actions[].nextStep",
    );
  }
  assert(Array.isArray(availability.enabled), "availability.enabled");

  const samples = [
    "contracts/samples/task.json",
    "contracts/samples/event.json",
    "contracts/samples/error.json",
    "contracts/samples/cancel.json",
    "contracts/samples/command.json",
    "contracts/samples/user-message.json",
    "contracts/samples/availability.json",
  ];
  const forbiddenFields = [
    "accessToken",
    "refreshToken",
    "clientSecret",
    "password",
    "authorization",
  ];
  for (const sample of samples) {
    const text = read(sample);
    for (const forbidden of forbiddenFields) {
      assert(
        !text.includes(forbidden),
        `${sample} must not contain credential field ${forbidden}`,
      );
    }
  }

  assert(
    tsSource.includes("attemptGeneration") && tsSource.includes("cancelRequested"),
    "TypeScript task mirror is missing task fields",
  );
  assert(tsSource.includes("evidence"), "TypeScript task mirror is missing evidence");
  for (const symbol of [
    "UserMessage",
    "MessageAction",
    "AvailabilityProjection",
    "ActionAvailability",
    "TaskStatePayload",
    "TaskProgressPayload",
    "TaskResultPayload",
  ]) {
    assert(
      tsSource.includes(symbol),
      `TypeScript contract mirror is missing ${symbol}`,
    );
  }

  console.log(
    `contract check passed (v${rustVersion}): ${samples.length} samples, ${forbiddenFields.length} security exclusions`,
  );
} catch (error) {
  console.error(`contract check failed: ${error.message}`);
  process.exitCode = 1;
}
