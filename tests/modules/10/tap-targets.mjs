// Prints the box height each control actually gets, per density.
// Run: node tests/modules/10/tap-targets.mjs
//
// Heights are resolved through `cascade.mjs` (real stylesheet, real cascade
// order, `var()` chains resolved), so this is the number a browser would apply,
// not the number a token table claims.
//
// Policy under test:
//   * primary actions and list rows  >= --tap-min (44px) at every density (D6)
//   * secondary / inline controls    >= --control-min (32px), not promised 44px

import { ELEMENTS, boxHeight } from "./cascade.mjs";
import { effectiveTokens } from "./tokens.mjs";

const PRIMARY = ["primaryButton", "listRow"];
const SECONDARY = ["secondaryButton", "smallButton", "iconButton", "tab", "segment"];

const rows = [];
let failures = 0;

for (const density of ["comfortable", "compact"]) {
  const env = { theme: "light", density };
  const tokens = effectiveTokens(env);
  const tapMin = Number.parseInt(tokens.get("--tap-min"), 10);
  const controlMin = Number.parseInt(tokens.get("--control-min"), 10);

  for (const [name, element] of Object.entries(ELEMENTS)) {
    const result = boxHeight(element, env);
    const promised = PRIMARY.includes(name) ? tapMin : SECONDARY.includes(name) ? controlMin : controlMin;
    const ok = result.value !== null && result.value >= promised;
    if (!ok) {
      failures += 1;
    }
    rows.push(
      `${density.padEnd(12)}${name.padEnd(16)}${String(result.value ?? "null").padStart(6)}${String(promised).padStart(9)}  ${ok ? "pass" : "FAIL"}   ${result.minHeightValue ?? result.heightValue ?? "-"}`,
    );
  }
}

console.log(
  `${"density".padEnd(12)}${"element".padEnd(16)}${"height".padStart(6)}${"promised".padStart(9)}  result   source`,
);
for (const row of rows) {
  console.log(row);
}

const env = { theme: "light" };
const tokens = effectiveTokens(env);
console.log("");
console.log(`--tap-min = ${tokens.get("--tap-min")}, --control-min = ${tokens.get("--control-min")}`);
console.log("promised: primary actions + list rows >= --tap-min; secondary controls >= --control-min");
console.log("");
console.log(`failures: ${failures}`);
process.exit(failures === 0 ? 0 : 1);
