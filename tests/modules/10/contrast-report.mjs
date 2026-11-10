// Prints the measured contrast table for every theme environment.
// Run: node tests/modules/10/contrast-report.mjs
// Exit code is 0 when every pair meets its threshold, 1 otherwise.

import {
  ENVIRONMENTS,
  NON_TEXT_PAIRS,
  TEXT_PAIRS,
  contrast,
  effectiveTokens,
  stylesPath,
  tokenHex,
} from "./tokens.mjs";

console.log(`styles: ${stylesPath}`);
console.log("");
console.log(
  `${"env".padEnd(27)}${"pair".padEnd(46)}${"ratio".padStart(6)}${"min".padStart(6)}  result`,
);

const rows = [];
let failed = 0;

function line(envName, label, ratio, minimum) {
  const ok = ratio >= minimum;
  if (!ok) {
    failed += 1;
  }
  rows.push(
    `${envName.padEnd(27)}${label.padEnd(46)}${ratio.toFixed(2).padStart(6)}${String(minimum).padStart(6)}  ${ok ? "pass" : "FAIL"}`,
  );
}

for (const env of ENVIRONMENTS) {
  const tokens = effectiveTokens(env);
  for (const [foreground, background, label] of TEXT_PAIRS) {
    const ratio = contrast(tokenHex(tokens, foreground), tokenHex(tokens, background));
    line(env.name, `${label} (${foreground}/${background})`, ratio, 4.5);
  }
  for (const [foreground, background, label] of NON_TEXT_PAIRS) {
    const ratio = contrast(tokenHex(tokens, foreground), tokenHex(tokens, background));
    line(env.name, `${label} (${foreground}/${background})`, ratio, 3);
  }
}

for (const row of rows) {
  console.log(row);
}

console.log("");
console.log(
  `environments: ${ENVIRONMENTS.length}, pairs: ${rows.length}, failures: ${failed}`,
);
process.exit(failed === 0 ? 0 : 1);
