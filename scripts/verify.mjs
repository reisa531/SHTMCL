#!/usr/bin/env node
// Frozen integration check for SHTMCL (owned by 00).
//
// Runs the checks that must stay green for any module change, in dependency
// order, and prints one table with real exit codes. It never edits files and
// never treats a skipped check as a pass.
//
// Usage:
//   node scripts/verify.mjs                 # all checks
//   node scripts/verify.mjs --only tsc,vite # subset, in the order given
//   node scripts/verify.mjs --json          # machine-readable summary
//
// Child stdio is inherited on purpose: redirection through pipes is blocked in
// some sandboxed harness profiles, and inherited output is what a reviewer wants
// to read anyway.

import { spawnSync } from "node:child_process";
import { dirname, join } from "node:path";
import { existsSync, readdirSync } from "node:fs";
import { fileURLToPath } from "node:url";

const root = join(dirname(fileURLToPath(import.meta.url)), "..");
const desktop = join(root, "apps", "desktop");

const nodeModules = {
  tsc: join(desktop, "node_modules", "typescript", "bin", "tsc"),
  vite: join(desktop, "node_modules", "vite", "bin", "vite.js"),
};

/** @type {{name: string, command: string, args: string[], cwd: string, needs?: string}[]} */
const CHECKS = [
  {
    name: "contracts",
    command: process.execPath,
    args: [join(root, "scripts", "check-contract.mjs")],
    cwd: root,
  },
  {
    name: "icons",
    command: process.execPath,
    args: [join(root, "scripts", "gen-icons.mjs"), "--check"],
    cwd: root,
  },
  {
    name: "namespaces",
    command: process.execPath,
    args: [join(root, "scripts", "check-codes.mjs")],
    cwd: root,
  },
  {
    name: "cargo-test",
    command: "cargo",
    args: ["test", "--workspace", "--no-fail-fast"],
    cwd: root,
  },
  {
    name: "cargo-build-desktop",
    command: "cargo",
    args: ["build", "-p", "shtmcl-desktop"],
    cwd: root,
  },
  {
    name: "tsc",
    command: process.execPath,
    args: [nodeModules.tsc, "--noEmit", "-p", "tsconfig.json"],
    cwd: desktop,
    needs: nodeModules.tsc,
  },
  {
    name: "vite",
    command: process.execPath,
    args: [nodeModules.vite, "build"],
    cwd: desktop,
    needs: nodeModules.vite,
  },
];

const argv = process.argv.slice(2);
const json = argv.includes("--json");
const onlyIndex = argv.indexOf("--only");
const only =
  onlyIndex >= 0 && argv[onlyIndex + 1]
    ? argv[onlyIndex + 1].split(",").map((value) => value.trim()).filter(Boolean)
    : null;

const selected = only ? CHECKS.filter((check) => only.includes(check.name)) : CHECKS;
const unknown = only ? only.filter((name) => !CHECKS.some((check) => check.name === name)) : [];
if (unknown.length > 0) {
  console.error(`unknown check(s): ${unknown.join(", ")}`);
  console.error(`known checks: ${CHECKS.map((check) => check.name).join(", ")}`);
  process.exit(2);
}

const results = [];
for (const check of selected) {
  if (check.needs && !existsSync(check.needs)) {
    results.push({ name: check.name, status: "missing", code: null });
    if (!json) {
      console.error(`\n[${check.name}] skipped: ${check.needs} is missing (run the package install first)`);
    }
    continue;
  }
  if (!json) {
    console.log(`\n=== ${check.name}: ${check.command} ${check.args.join(" ")}`);
  }
  const started = Date.now();
  const result = spawnSync(check.command, check.args, {
    cwd: check.cwd,
    stdio: "inherit",
    env: process.env,
  });
  const code = result.error ? 1 : (result.status ?? 1);
  results.push({
    name: check.name,
    status: code === 0 ? "pass" : "fail",
    code,
    ms: Date.now() - started,
    error: result.error?.message,
  });
}

// Module test suites live per module and are run from their own directory, so a
// wrapper that uses relative file names still resolves. Each module reports its
// own row: a failure names the module instead of hiding behind "node tests".
const runModuleTests = !only || only.includes("modules") || only.includes("modules:*");
const moduleDirs = runModuleTests ? discoverModuleTests() : [];
for (const dir of moduleDirs) {
  const name = `modules/${dir.name}`;
  if (!json) {
    console.log(`\n=== ${name}: (cwd=${dir.path}) ${dir.command} ${dir.args.join(" ")}`);
  }
  const started = Date.now();
  const result = spawnSync(dir.command, dir.args, {
    cwd: dir.path,
    stdio: "inherit",
    env: process.env,
  });
  const code = result.error ? 1 : (result.status ?? 1);
  results.push({
    name,
    status: code === 0 ? "pass" : "fail",
    code,
    ms: Date.now() - started,
    error: result.error?.message,
  });
}

function discoverModuleTests() {
  const base = join(root, "tests", "modules");
  let entries;
  try {
    entries = readdirSync(base, { withFileTypes: true });
  } catch {
    return [];
  }
  const dirs = [];
  for (const entry of entries) {
    if (!entry.isDirectory()) continue;
    const path = join(base, entry.name);
    let files;
    try {
      files = readdirSync(path);
    } catch {
      continue;
    }
    if (files.includes("run.mjs")) {
      dirs.push({ name: entry.name, path, command: process.execPath, args: ["run.mjs"] });
      continue;
    }
    const tests = files.filter((file) => file.endsWith(".test.mjs")).sort();
    if (tests.length > 0) {
      dirs.push({ name: entry.name, path, command: process.execPath, args: ["--test", ...tests] });
    }
  }
  return dirs;
}

const failed = results.filter((row) => row.status !== "pass");
if (json) {
  console.log(JSON.stringify({ at: new Date().toISOString(), results }, null, 2));
} else {
  console.log("\nname                     status   exit   ms");
  for (const row of results) {
    const exit = row.code === null ? "-" : String(row.code);
    console.log(`${row.name.padEnd(24)} ${row.status.padEnd(8)} ${exit.padEnd(6)} ${row.ms ?? "-"}`);
  }
  if (failed.length > 0) {
    console.log(`\n${failed.length} check(s) did not pass: ${failed.map((row) => row.name).join(", ")}`);
  }
}

process.exit(failed.length === 0 ? 0 : 1);
