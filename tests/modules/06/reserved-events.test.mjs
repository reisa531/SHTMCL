// Contract check for the events module 06 publishes.
//
// The Rust suite (`cargo test -p shtmcl-core --test 06_rooms`) is the authority for
// behaviour; this test guards the *shape* that 10/11/00 consume, and it runs without
// a Rust toolchain:
//
//   node --test tests/modules/06/reserved-events.test.mjs
//
// It asserts `contracts/domain-v1.md` section 12: required payload fields, the
// allowed enum values, `transport` only on `connected`, and no credential, invite
// digest or candidate address in any payload.

import { readFileSync } from "node:fs";
import { dirname, join } from "node:path";
import { fileURLToPath } from "node:url";
import { strict as assert } from "node:assert";
import test from "node:test";

const here = dirname(fileURLToPath(import.meta.url));
const document = JSON.parse(readFileSync(join(here, "reserved-events.json"), "utf8"));
const samples = document.samples;

const APPROVAL_STATES = ["requested", "approved", "denied", "left", "removed"];
const CONNECTION_STATES = [
  "gathering",
  "connecting",
  "connected",
  "reconnecting",
  "disconnected",
  "failed",
];
const TRANSPORTS = ["direct", "relay"];
const FORBIDDEN_KEY_FRAGMENTS = [
  "secret",
  "token",
  "credential",
  "password",
  "cookie",
  "candidate",
  "address",
];

function walk(value, path, problems) {
  if (Array.isArray(value)) {
    value.forEach((item, index) => walk(item, `${path}[${index}]`, problems));
    return;
  }
  if (value && typeof value === "object") {
    for (const [key, child] of Object.entries(value)) {
      const lower = key.toLowerCase();
      for (const fragment of FORBIDDEN_KEY_FRAGMENTS) {
        if (lower.includes(fragment)) {
          problems.push(`${path}.${key} uses a forbidden '${fragment}' field`);
        }
      }
      if (lower.includes("digest") && (lower.includes("invite") || lower.includes("secret"))) {
        problems.push(`${path}.${key} carries invite digest material`);
      }
      walk(child, `${path}.${key}`, problems);
    }
    return;
  }
  if (typeof value === "string") {
    if (value.includes("://")) {
      problems.push(`${path} contains a URL`);
    }
    const parts = value.split(".");
    if (parts.length === 4 && parts.every((part) => /^\d+$/.test(part))) {
      problems.push(`${path} looks like a network address`);
    }
  }
}

test("every sample declares one of the known kinds", () => {
  const known = new Set([
    "membership.changed",
    "connection.changed",
    "rooms.room.changed",
    "rooms.invite.changed",
  ]);
  for (const sample of samples) {
    assert.ok(known.has(sample.kind), `unexpected kind ${sample.kind}`);
  }
});

test("membership.changed always carries a valid approvalState", () => {
  const memberships = samples.filter((sample) => sample.kind === "membership.changed");
  assert.ok(memberships.length >= 5, "expected every approval state to be covered");
  for (const sample of memberships) {
    const state = sample.payload.approvalState;
    assert.ok(
      APPROVAL_STATES.includes(state),
      `approvalState ${state} is not in the frozen enum`,
    );
  }
  const covered = new Set(memberships.map((sample) => sample.payload.approvalState));
  for (const state of APPROVAL_STATES) {
    assert.ok(covered.has(state), `no sample covers approvalState ${state}`);
  }
});

test("connection.changed always carries a valid state", () => {
  const connections = samples.filter((sample) => sample.kind === "connection.changed");
  for (const sample of connections) {
    const state = sample.payload.state;
    assert.ok(
      CONNECTION_STATES.includes(state),
      `connection state ${state} is not in the frozen enum`,
    );
  }
  const covered = new Set(connections.map((sample) => sample.payload.state));
  for (const state of ["gathering", "connecting", "connected", "disconnected", "failed"]) {
    assert.ok(covered.has(state), `no sample covers connection state ${state}`);
  }
});

test("transport appears only on connected and only with a real value", () => {
  for (const sample of samples) {
    if (sample.kind !== "connection.changed") continue;
    const { state, transport } = sample.payload;
    if (state === "connected") {
      assert.ok(
        TRANSPORTS.includes(transport),
        `connected must report a real transport, got ${transport}`,
      );
    } else {
      assert.equal(
        transport,
        undefined,
        `state ${state} must not predict a transport`,
      );
    }
  }
});

test("terminal membership states carry the matching marker", () => {
  // Frozen in 00 handoff §3.3: leaving is the member's own action, being removed is the
  // owner's. The two markers may never stand in for each other.
  for (const sample of samples) {
    if (sample.kind !== "membership.changed") continue;
    const { approvalState, evidence } = sample.payload;
    if (approvalState === "left") {
      assert.deepEqual(
        evidence,
        ["membership_left"],
        "left must publish membership_left",
      );
    }
    if (approvalState === "removed") {
      assert.deepEqual(
        evidence,
        ["membership_removed"],
        "removed must publish membership_removed",
      );
    }
    if (approvalState === "left" || approvalState === "removed") {
      const other = approvalState === "left" ? "membership_removed" : "membership_left";
      assert.ok(
        !evidence.includes(other),
        `${approvalState} must not also claim ${other}`,
      );
    }
  }
});

test("no payload carries credentials, invite material or addresses", () => {
  const problems = [];
  for (const sample of samples) {
    walk(sample.payload, sample.kind, problems);
  }
  assert.deepEqual(problems, []);
});

test("no payload carries a secret-shaped hex blob", () => {
  // Invite secrets are 32 OS-random bytes (64 lowercase hex characters) and the stored
  // digest is also 64 hex characters: neither may appear in an event payload, nor in
  // this fixture set, which consumers read as documentation.
  const hexish = /[0-9a-f]{32,}/;
  const offenders = [];
  const visit = (value, path) => {
    if (typeof value === "string") {
      if (hexish.test(value)) offenders.push(`${path}: ${value}`);
      return;
    }
    if (Array.isArray(value)) {
      value.forEach((item, index) => visit(item, `${path}[${index}]`));
      return;
    }
    if (value && typeof value === "object") {
      for (const [key, child] of Object.entries(value)) visit(child, `${path}.${key}`);
    }
  };
  for (const sample of samples) visit(sample.payload, sample.kind);
  assert.deepEqual(offenders, []);
});

test("module-local kinds stay inside the rooms namespace", () => {
  for (const sample of samples) {
    if (sample.kind === "membership.changed" || sample.kind === "connection.changed") {
      continue;
    }
    assert.ok(
      sample.kind.startsWith("rooms."),
      `${sample.kind} is not namespaced by the module`,
    );
  }
});
