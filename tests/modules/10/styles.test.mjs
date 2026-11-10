// Module 10 · stylesheet token discipline.
//
// Three escapes the previous round allowed: a token declared but never used
// (D6), raw dimensions in component rules (D7), and a media override that could
// never win the cascade (D4). Each is now an assertion over the real file.

import test from "node:test";
import assert from "node:assert/strict";

import { css, effectiveTokens, tokenBlocks, stylesPath } from "./tokens.mjs";
import { ELEMENTS, boxHeight } from "./cascade.mjs";

const declared = new Set();
for (const block of tokenBlocks) {
  for (const name of block.tokens.keys()) {
    declared.add(name);
  }
}

test("every declared token is referenced", () => {
  const referenced = new Set(
    [...css.matchAll(/var\((--[a-z0-9-]+)\)/g)].map((match) => match[1]),
  );
  const unused = [...declared].filter((name) => !referenced.has(name)).sort();
  assert.deepEqual(unused, [], `tokens declared but never used: ${unused.join(", ")}`);
});

test("main actions and list rows use the touch-target token (D6)", () => {
  assert.match(
    css,
    /--control-primary-h:\s*var\(--tap-min\)/,
    "--control-primary-h must resolve to --tap-min",
  );
  assert.match(
    css,
    /\.btn--primary\s*\{[^}]*min-height:\s*var\(--control-primary-h\)/,
    "primary button height must come from the token",
  );
  assert.match(
    css,
    /\.list__row\s*\{[^}]*min-height:\s*var\(--tap-min\)/,
    "list rows must use the touch-target token",
  );
});

test("the touch target survives compact density (D6 follow-up)", () => {
  for (const density of ["comfortable", "compact"]) {
    const env = { theme: "light", density };
    const tokens = effectiveTokens(env);
    const tapMin = Number.parseInt(tokens.get("--tap-min"), 10);
    const controlH = Number.parseInt(tokens.get("--control-h"), 10);

    const primary = boxHeight(ELEMENTS.primaryButton, env);
    const row = boxHeight(ELEMENTS.listRow, env);

    assert.equal(
      primary.value,
      tapMin,
      `${density}: primary button must be ${tapMin}px, got ${primary.value} (${primary.minHeightValue})`,
    );
    assert.ok(
      primary.value !== controlH,
      `${density}: primary button fell back to --control-h (${controlH}px)`,
    );
    assert.ok(
      row.value !== null && row.value >= tapMin,
      `${density}: list row must be >= ${tapMin}px, got ${row.value}`,
    );
  }
});

test("secondary controls keep the documented floor and are not promised 44px", () => {
  const secondary = ["secondaryButton", "smallButton", "iconButton", "tab", "segment"];
  for (const density of ["comfortable", "compact"]) {
    const env = { theme: "light", density };
    const tokens = effectiveTokens(env);
    const controlMin = Number.parseInt(tokens.get("--control-min"), 10);
    for (const name of secondary) {
      const { value } = boxHeight(ELEMENTS[name], env);
      assert.ok(
        value !== null && value >= controlMin,
        `${density} ${name}: ${value} < --control-min (${controlMin})`,
      );
    }
  }
});

test("component rules carry no raw pixel sizes (D7)", () => {
  const offenders = [];
  const lines = css.split("\n");
  for (const [index, line] of lines.entries()) {
    const trimmed = line.trim();
    if (trimmed.startsWith("--")) continue; // token declaration
    if (trimmed.startsWith("@")) continue; // media prelude / at-rule
    if (trimmed.startsWith("*")) continue;
    if (trimmed.startsWith("/*") || trimmed.startsWith("//")) continue; // comment
    for (const match of line.matchAll(/(\d+(?:\.\d+)?)px/g)) {
      const value = Number(match[1]);
      if (value >= 3) {
        offenders.push(`line ${index + 1}: ${trimmed}`);
        break;
      }
    }
  }
  assert.deepEqual(offenders, [], `raw pixel sizes:\n${offenders.join("\n")}`);
});

test("letter spacing comes from tracking tokens (D7)", () => {
  const offenders = [];
  for (const [index, line] of css.split("\n").entries()) {
    if (!/letter-spacing:/.test(line)) continue;
    if (/^\s*--[a-z0-9-]+\s*:/.test(line)) continue;
    if (!/letter-spacing:\s*var\(--[a-z0-9-]+\)/.test(line)) {
      offenders.push(`line ${index + 1}: ${line.trim()}`);
    }
  }
  assert.deepEqual(offenders, [], `raw letter spacing:\n${offenders.join("\n")}`);
});

test("the high-contrast block covers follow-system selectors (D4)", () => {
  const contrastBlocks = tokenBlocks.filter(
    (block) =>
      block.media.some((query) => /prefers-contrast:\s*more/i.test(query)) &&
      !block.media.some((query) => /prefers-color-scheme/i.test(query)),
  );
  assert.ok(contrastBlocks.length > 0, "no prefers-contrast: more token block");

  const selectors = contrastBlocks.flatMap((block) => block.selectors);
  assert.ok(
    selectors.includes(':root[data-theme="auto"]'),
    `follow-system branch missing from the high-contrast block: ${selectors.join(", ")}`,
  );
  assert.ok(selectors.includes(':root[data-theme="dark"]'));

  const nestedDark = tokenBlocks.filter(
    (block) =>
      block.media.some((query) => /prefers-contrast:\s*more/i.test(query)) &&
      block.media.some((query) => /prefers-color-scheme:\s*dark/i.test(query)),
  );
  assert.ok(nestedDark.length > 0, "no dark branch inside prefers-contrast: more");
  const nestedSelectors = nestedDark.flatMap((block) => block.selectors);
  assert.ok(nestedSelectors.includes(':root[data-theme="auto"]'));
  assert.ok(nestedSelectors.includes(":root:not([data-theme])"));
});

test("the topbar separator is not rendered with a border colour (D5)", () => {
  assert.match(
    css,
    /\.topbar__separator\s*\{[^}]*color:\s*var\(--subtle\)/,
    "separator glyph must use a text token",
  );
});

test("effective tokens resolve without a data-theme attribute", () => {
  const tokens = effectiveTokens({ theme: "none", colorScheme: "light" });
  assert.ok(tokens.get("--text"), "base tokens apply with no attribute");
  assert.ok(stylesPath.endsWith("styles.css"));
});
