#!/usr/bin/env node
// Namespace check for stable identifiers (owned by 00).
//
// Error codes, command names, event kinds and task phases all share one dotted
// syntax on purpose. That only stays usable if the first segment is the owner's
// namespace, so this script reads every `const NAME: &str = "a.b.c"` declaration
// under the module sources and checks it. It is deliberately declaration-based:
// matching loose string literals would also catch class names, file names and
// Maven coordinates.
//
// It also reports a literal declared twice, because that usually means a shared
// definition was copied instead of imported.
//
// Usage: node scripts/check-codes.mjs [--json]

import { readFileSync, readdirSync, statSync } from "node:fs";
import { basename, dirname, join, relative, sep } from "node:path";
import { fileURLToPath } from "node:url";

const root = join(dirname(fileURLToPath(import.meta.url)), "..");

// Owner namespaces: one per core module plus the shared envelope owners.
const SHARED_OWNERS = ["task", "command", "module", "internal"];
const MODULE_DIRS = [
  "instances",
  "launch",
  "downloads",
  "content",
  "accounts",
  "rooms",
  "servers",
  "diagnostics",
];
// 09 is an optional research PoC with no `ModuleId`. Its frozen home is
// `experiments/distribution/` and it is never registered in `CoreApp`, but its
// identifier family is still namespaced, so the prefix is legitimate anywhere.
const RESEARCH_OWNERS = ["distribution"];
const OWNERS = new Set([...SHARED_OWNERS, ...MODULE_DIRS, ...RESEARCH_OWNERS]);

// Cross-module event kinds frozen in `contracts/domain-v1.md` §12. Their names
// are consumed by the UX layer, so they are deliberately not module-prefixed;
// renaming them would break every consumer. Add here only with a contract change.
const RESERVED_CROSS_MODULE = new Set([
  "membership.changed",
  "connection.changed",
  "account.authorization.changed",
]);

// `payload.command` strings may live next to a module but describe a family in
// that module's namespace, so no extra allowance is needed. Anything else with a
// foreign first segment is a real ownership error.

const SOURCE_ROOTS = ["crates/core/src", "crates/domain/src"];
const EXTENSIONS = new Set([".rs"]);

const DECLARATION =
  /(?:\bpub\s+)?\bconst\s+([A-Z][A-Z0-9_]*)\s*:\s*&(?:\s*'static\s*)?str\s*=\s*"([a-z][a-z0-9._-]{2,63})"/g;

// Not every dotted string is a namespaced identifier: schema ids, archive names
// and file names are also string constants. A namespaced identifier is
// `segment.segment[.segment]` where the first segment is a bare lowercase word
// (no `-`, which excludes `content-lock.v1`) and the value does not end in a
// file/schema suffix.
const BARE_SEGMENT = /^[a-z][a-z0-9]*$/;
const NON_NAMESPACE_SUFFIXES = [
  ".json",
  ".v1",
  ".v2",
  ".v3",
  ".mrpack",
  ".jar",
  ".dat",
  ".tmp",
  ".toml",
  ".properties",
  ".lock",
  ".txt",
  ".log",
  ".index",
];

function looksNamespaced(value) {
  if (!value.includes(".")) return false;
  const first = value.split(".")[0];
  if (!BARE_SEGMENT.test(first)) return false;
  if (!first.includes("_") && first.length < 2) return false;
  return !NON_NAMESPACE_SUFFIXES.some((suffix) => value.endsWith(suffix));
}

function walk(dir, files = []) {
  let entries;
  try {
    entries = readdirSync(dir);
  } catch {
    return files;
  }
  for (const entry of entries) {
    const full = join(dir, entry);
    const info = statSync(full, { throwIfNoEntry: false });
    if (!info) continue;
    if (info.isDirectory()) {
      walk(full, files);
    } else if (EXTENSIONS.has(full.slice(full.lastIndexOf(".")))) {
      files.push(full);
    }
  }
  return files;
}

const files = SOURCE_ROOTS.flatMap((dir) => walk(join(root, dir)));
const declarations = [];
const violations = [];

for (const file of files) {
  const text = readFileSync(file, "utf8");
  const lines = text.split("\n");
  lines.forEach((line, index) => {
    DECLARATION.lastIndex = 0;
    let match;
    while ((match = DECLARATION.exec(line)) !== null) {
      const [, name, value] = match;
      if (!looksNamespaced(value)) continue;
      const owner = value.split(".")[0];
      const where = `${relative(root, file).split(sep).join("/")}:${index + 1}`;
      declarations.push({ name, value, owner, where });
      if (!OWNERS.has(owner) && !RESERVED_CROSS_MODULE.has(value)) {
        violations.push({
          kind: "foreign-namespace",
          where,
          name,
          value,
          message: `"${value}" starts with "${owner}", which is not a module or shared namespace (${[...OWNERS].join(", ")})`,
        });
      }
    }
  });
}

// A value declared twice suggests a copied definition rather than a shared one.
const byValue = new Map();
for (const declaration of declarations) {
  const list = byValue.get(declaration.value) ?? [];
  list.push(declaration);
  byValue.set(declaration.value, list);
}
const duplicates = [...byValue.entries()]
  .filter(([, list]) => list.length > 1)
  .map(([value, list]) => ({ value, where: list.map((item) => item.where) }));

// Source files that no `mod` declaration reaches are never compiled, so nothing
// they claim is verified. Report them: an uncompiled module that "exists" is the
// most expensive kind of dead code, because a reviewer may count it as done.
const unreachable = findUnreachableSources();

function findUnreachableSources() {
  const unreachable = [];
  for (const crateRoot of ["crates/core/src", "crates/domain/src", "crates/platform/src"]) {
    const entry = join(root, crateRoot, "lib.rs");
    const reachable = new Set();
    const visit = (file) => {
      if (reachable.has(file)) return;
      reachable.add(file);
      let text;
      try {
        text = readFileSync(file, "utf8");
      } catch {
        return;
      }
      // Rust resolves children of `store.rs` under `store/`, while crate
      // roots and `mod.rs` resolve children beside the current file.
      const dir = ["lib.rs", "main.rs", "mod.rs"].includes(basename(file))
        ? dirname(file)
        : join(dirname(file), basename(file, ".rs"));
      const MOD = /(?:^|\n)\s*(?:pub(?:\([^)]*\))?\s+)?mod\s+([a-z0-9_]+)\s*;/g;
      let match;
      while ((match = MOD.exec(text)) !== null) {
        for (const candidate of [
          join(dir, `${match[1]}.rs`),
          join(dir, match[1], "mod.rs"),
        ]) {
          const info = statSync(candidate, { throwIfNoEntry: false });
          if (info?.isFile()) {
            visit(candidate);
            break;
          }
        }
      }
    };
    const info = statSync(entry, { throwIfNoEntry: false });
    if (!info?.isFile()) continue;
    visit(entry);
    for (const file of walk(join(root, crateRoot))) {
      if (!reachable.has(file)) {
        unreachable.push(relative(root, file).split(sep).join("/"));
      }
    }
  }
  return unreachable;
}

const summary = {
  files: files.length,
  declarations: declarations.length,
  owners: [...OWNERS].sort(),
  violations,
  duplicates,
  unreachable,
};

if (process.argv.includes("--list")) {
  // A readable inventory for reviewers: who declares which stable identifier.
  // It is generated, never hand-maintained, so it cannot drift.
  const byOwner = new Map();
  for (const declaration of declarations) {
    const list = byOwner.get(declaration.owner) ?? [];
    list.push(declaration);
    byOwner.set(declaration.owner, list);
  }
  console.log("# Stable identifier catalog (generated)\n");
  console.log(
    "Regenerate with `node scripts/check-codes.mjs --list > docs/development/handoffs/00/CODE-CATALOG.md`.\n",
  );
  for (const owner of [...byOwner.keys()].sort()) {
    console.log(`## ${owner} (${byOwner.get(owner).length})\n`);
    console.log("| value | const | where |");
    console.log("| --- | --- | --- |");
    for (const declaration of byOwner.get(owner).sort((a, b) => a.value.localeCompare(b.value))) {
      console.log(`| \`${declaration.value}\` | \`${declaration.name}\` | ${declaration.where} |`);
    }
    console.log("");
  }
  process.exitCode = violations.length === 0 ? 0 : 1;
} else if (process.argv.includes("--json")) {
  console.log(JSON.stringify(summary, null, 2));
} else {
  console.log(
    `namespace check: ${declarations.length} declarations in ${files.length} files, ${OWNERS.size} allowed namespaces`,
  );
  for (const violation of violations) {
    console.error(`  BAD  ${violation.where}  ${violation.name}  ${violation.message}`);
  }
  for (const duplicate of duplicates) {
    // A phase may legitimately share a name with a command ("content.plan");
    // this is a readability warning, not a dispatch error.
    console.error(`  WARN ${duplicate.value} declared at ${duplicate.where.join(" and ")}`);
  }
  for (const file of unreachable) {
    console.error(`  DEAD ${file} is not reachable from any mod declaration, so it is never compiled`);
  }
  if (violations.length === 0) {
    console.log("  every namespaced identifier stays inside an owner namespace");
  }
}

process.exitCode = violations.length === 0 ? 0 : 1;
