#!/usr/bin/env node
// 08 · redaction regression sample: shape check.
//
// The real assertion lives in the Rust suite
// (`crates/core/tests/08_diagnostics.rs`), which runs the shipped `redact_line`
// against this same cases file and scans the produced bundle. This test keeps the
// sample honest so a future edit cannot silently drop a form or a category — it is
// the file that was too narrow to catch D08-1 (the command-line flag form).
//
// Run: node --test tests/modules/08/redaction.test.mjs

import assert from "node:assert/strict";
import { readFileSync } from "node:fs";
import { dirname, join } from "node:path";
import { fileURLToPath } from "node:url";
import test from "node:test";

const here = dirname(fileURLToPath(import.meta.url));
const cases = JSON.parse(readFileSync(join(here, "redaction-cases.json"), "utf8"));
const sampleLog = readFileSync(join(here, "fixtures", "credentials-sample.log"), "utf8");

// Every shape a credential really appears in. A form without a case is a leak
// waiting to happen, so the list is asserted, not just documented.
const REQUIRED_CATEGORIES = [
  "authorization",
  "token",
  "flag",
  "callback",
  "credential",
  "shape",
];

test("the cases file keeps its schema and every required category", () => {
  assert.equal(cases.schema, "diagnostics-redaction-cases.v2");
  assert.equal(cases.owner, "08");
  assert.ok(Array.isArray(cases.cases) && cases.cases.length >= 12);
  for (const category of REQUIRED_CATEGORIES) {
    assert.ok(
      Object.hasOwn(cases.categories, category),
      `category ${category} must stay documented`,
    );
    assert.ok(
      cases.cases.some((entry) => entry.category === category),
      `a case must still cover ${category}`,
    );
  }
});

test("the flag form covers space, equals, snake and JVM spellings", () => {
  const flagCases = cases.cases.filter((entry) => entry.category === "flag");
  const lines = flagCases.map((entry) => entry.line).join("\n");
  assert.ok(/--accessToken \S+/.test(lines), "space form missing");
  assert.ok(/--access-token=/.test(lines), "equals form missing");
  assert.ok(/--access_token /.test(lines), "snake form missing");
  assert.ok(/-DaccessToken=/.test(lines), "JVM -D form missing");
  for (const entry of flagCases) {
    assert.ok(
      entry.mustNotContain.length > 0,
      `${entry.id}: a flag case without a forbidden value proves nothing`,
    );
  }
});

test("every case lists what must go and what must stay", () => {
  const ids = new Set();
  for (const entry of cases.cases) {
    assert.equal(typeof entry.id, "string");
    assert.ok(!ids.has(entry.id), `duplicate case id ${entry.id}`);
    ids.add(entry.id);
    assert.equal(typeof entry.line, "string");
    assert.ok(Array.isArray(entry.mustNotContain));
    assert.ok(Array.isArray(entry.mustContain));
    for (const forbidden of entry.mustNotContain) {
      assert.ok(
        entry.line.includes(forbidden),
        `${entry.id}: the case must actually contain ${forbidden}`,
      );
    }
    if (entry.mustNotContain.length === 0) {
      assert.ok(
        entry.mustContain.length > 0,
        `${entry.id}: a kept case must name what stays`,
      );
    }
  }
});

test("the fake session log still carries every credential marker", () => {
  const markers = {
    authorization: /Authorization: Bearer /i,
    flag: /--accessToken \S+/,
    token: /(accessToken=|"access_token"|refresh_token)/,
    callback: /[?&]code=/,
    credential: /(password=|client_secret=)/,
    shape: /sha1=[0-9a-f]{40}/,
    jwt: /eyJ[A-Za-z0-9_-]+/,
  };
  for (const [category, pattern] of Object.entries(markers)) {
    assert.ok(pattern.test(sampleLog), `the sample log lost its ${category} marker`);
  }
  assert.ok(sampleLog.includes("Duplicate mods found:"), "the sample keeps a rule hit");
  assert.ok(sampleLog.includes("OutOfMemoryError"), "the sample keeps a rule hit");
});
