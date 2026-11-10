// Module 10 · token and contrast checks.
//
// Reads the real stylesheet and models the cascade, so a token edit that breaks
// WCAG AA — or a media-query override that never applies — fails here instead of
// being caught by eye. Thresholds: 4.5:1 for text, 3:1 for focus indicators and
// control boundaries (WCAG 1.4.3 / 1.4.11).

import test from "node:test";
import assert from "node:assert/strict";

import {
  ENVIRONMENTS,
  NON_TEXT_PAIRS,
  TEXT_PAIRS,
  contrast,
  css,
  effectiveTokens,
  stylesPath,
  tokenHex,
} from "./tokens.mjs";

for (const env of ENVIRONMENTS) {
  test(`${env.name}: WCAG AA for text`, () => {
    const tokens = effectiveTokens(env);
    for (const [foreground, background] of TEXT_PAIRS) {
      const ratio = contrast(tokenHex(tokens, foreground), tokenHex(tokens, background));
      assert.ok(
        ratio >= 4.5,
        `${env.name} ${foreground} on ${background} = ${ratio.toFixed(2)} (< 4.5)`,
      );
    }
  });

  test(`${env.name}: 3:1 for focus and control borders`, () => {
    const tokens = effectiveTokens(env);
    for (const [foreground, background] of NON_TEXT_PAIRS) {
      const ratio = contrast(tokenHex(tokens, foreground), tokenHex(tokens, background));
      assert.ok(
        ratio >= 3,
        `${env.name} ${foreground} on ${background} = ${ratio.toFixed(2)} (< 3)`,
      );
    }
  });
}

test("follow-system mirrors the explicit theme it resolves to", () => {
  const autoLight = effectiveTokens({ theme: "auto", colorScheme: "light" });
  const explicitLight = effectiveTokens({ theme: "light" });
  const autoDark = effectiveTokens({ theme: "auto", colorScheme: "dark" });
  const explicitDark = effectiveTokens({ theme: "dark" });

  for (const name of explicitLight.keys()) {
    assert.equal(autoLight.get(name), explicitLight.get(name), `auto light ${name}`);
  }
  for (const name of explicitDark.keys()) {
    assert.equal(autoDark.get(name), explicitDark.get(name), `auto dark ${name}`);
  }
});

test("high contrast applies to follow-system + system dark (D4)", () => {
  const normalDark = effectiveTokens({ theme: "auto", colorScheme: "dark" });
  const contrastDark = effectiveTokens({
    theme: "auto",
    colorScheme: "dark",
    contrast: "more",
  });
  const explicitContrastDark = effectiveTokens({
    theme: "dark",
    colorScheme: "dark",
    contrast: "more",
  });

  for (const name of ["--muted", "--subtle", "--line", "--line-strong", "--control-border"]) {
    assert.notEqual(
      contrastDark.get(name),
      normalDark.get(name),
      `${name} did not switch to the high-contrast value`,
    );
    assert.equal(
      contrastDark.get(name),
      explicitContrastDark.get(name),
      `${name} differs between follow-system and explicit dark under high contrast`,
    );
  }

  // The same must hold when no `data-theme` attribute is present at all.
  const noAttribute = effectiveTokens({
    theme: "none",
    colorScheme: "dark",
    contrast: "more",
  });
  assert.equal(noAttribute.get("--muted"), explicitContrastDark.get("--muted"));
});

test("high contrast applies to follow-system + system light", () => {
  const normal = effectiveTokens({ theme: "auto", colorScheme: "light" });
  const enhanced = effectiveTokens({
    theme: "auto",
    colorScheme: "light",
    contrast: "more",
  });
  const explicit = effectiveTokens({
    theme: "light",
    colorScheme: "light",
    contrast: "more",
  });

  for (const name of ["--muted", "--subtle", "--line", "--line-strong", "--control-border"]) {
    assert.notEqual(enhanced.get(name), normal.get(name), `${name} not enhanced`);
    assert.equal(enhanced.get(name), explicit.get(name), `${name} differs from explicit light`);
  }
});

test("colour literals live only in token declarations", () => {
  assert.ok(stylesPath.endsWith("styles.css"));
  const offenders = [];
  const lines = css.split("\n");
  for (const [index, line] of lines.entries()) {
    if (!/#[0-9a-f]{3,8}\b/i.test(line)) continue;
    if (/^\s*--[a-z0-9-]+\s*:/.test(line)) continue;
    offenders.push(`line ${index + 1}: ${line.trim()}`);
  }
  assert.deepEqual(
    offenders,
    [],
    `raw colour outside the token layer:\n${offenders.join("\n")}`,
  );
});

test("tokens cover the categories the task requires", () => {
  const light = effectiveTokens({ theme: "light" });
  const dark = effectiveTokens({ theme: "dark" });

  const shared = [
    "--font-family",
    "--font-base",
    "--font-page-title",
    "--space-1",
    "--space-6",
    "--radius-md",
    "--radius-pill",
    "--motion-fast",
    "--motion",
    "--ease",
    "--control-h",
    "--tap-min",
  ];
  for (const name of shared) {
    assert.ok(light.has(name), `missing token ${name}`);
  }

  const themed = [
    "--bg",
    "--surface",
    "--surface-soft",
    "--surface-muted",
    "--text",
    "--muted",
    "--subtle",
    "--line",
    "--line-strong",
    "--control-border",
    "--primary",
    "--on-primary",
    "--focus",
    "--info",
    "--ok",
    "--warn",
    "--danger",
    "--shadow",
    "--shadow-soft",
  ];
  for (const name of themed) {
    assert.ok(light.has(name), `light theme missing ${name}`);
    assert.ok(dark.has(name), `dark theme missing ${name}`);
  }
});
