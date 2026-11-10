// Reads the design tokens straight out of the real stylesheet and models the
// cascade for a given environment, so contrast checks and the high-contrast
// regression use the values a browser would actually apply.
//
// The model is deliberately small but honest: it walks the real file, keeps the
// order of token blocks, records the enclosing `@media` conditions and the
// selector specificity of each block, then applies matching blocks in
// (specificity, source order) order — which is what CSS does for these simple
// `:root` selectors. It exists because reading raw blocks by name is what let
// defect D4 (follow-system + system dark ignoring `prefers-contrast: more`)
// slip through.

import { readFileSync } from "node:fs";
import path from "node:path";
import { fileURLToPath } from "node:url";

const here = path.dirname(fileURLToPath(import.meta.url));
export const stylesPath = path.resolve(
  here,
  "../../../apps/desktop/src/styles.css",
);

export const css = readFileSync(stylesPath, "utf8");

function stripComments(source) {
  return source.replace(/\/\*[\s\S]*?\*\//g, "");
}

function tokensOf(block) {
  const map = new Map();
  for (const match of block.matchAll(/(--[a-z0-9-]+)\s*:\s*([^;]+);/g)) {
    map.set(match[1], match[2].trim());
  }
  return map;
}

/** Rough selector weight: pseudo-classes and attribute selectors count. */
function specificity(selector) {
  const pseudo = (selector.match(/:[a-z-]+/g) ?? []).length;
  const attributes = (selector.match(/\[/g) ?? []).length;
  return pseudo + attributes;
}

function splitSelectors(prelude) {
  return prelude
    .split(",")
    .map((part) => part.trim())
    .filter(Boolean);
}

function walk(text, mediaStack, out) {
  let index = 0;
  while (index < text.length) {
    const open = text.indexOf("{", index);
    if (open === -1) {
      return;
    }
    const prelude = text.slice(index, open).trim();
    let depth = 0;
    let cursor = open;
    for (; cursor < text.length; cursor += 1) {
      if (text[cursor] === "{") depth += 1;
      if (text[cursor] === "}") {
        depth -= 1;
        if (depth === 0) {
          break;
        }
      }
    }
    const body = text.slice(open + 1, cursor);

    if (prelude.startsWith("@")) {
      walk(body, [...mediaStack, prelude], out);
    } else {
      // Keep density/motion blocks too: they redefine control-size tokens, and
      // the tap-target assertions need the values compact density really uses.
      const selectors = splitSelectors(prelude).filter((selector) =>
        selector.startsWith(":root"),
      );
      if (selectors.length > 0) {
        out.push({
          selectors,
          media: mediaStack.slice(),
          tokens: tokensOf(body),
          order: out.length,
        });
      }
    }
    index = cursor + 1;
  }
}

export const tokenBlocks = (() => {
  const blocks = [];
  walk(stripComments(css), [], blocks);
  return blocks;
})();

function mediaMatches(query, env) {
  const text = query.toLowerCase();
  if (text.includes("prefers-color-scheme: dark")) {
    return env.colorScheme === "dark";
  }
  if (text.includes("prefers-color-scheme: light")) {
    return env.colorScheme === "light";
  }
  if (text.includes("prefers-contrast: more")) {
    return env.contrast === "more";
  }
  if (text.includes("prefers-contrast: less")) {
    return env.contrast === "less";
  }
  // Reduced motion, forced colours and width breakpoints carry no token values.
  return false;
}

function selectorMatches(selector, env) {
  const themeAttr = selector.match(/\[data-theme="([a-z]+)"\]/);
  if (themeAttr && env.theme !== themeAttr[1]) {
    return false;
  }
  if (selector.includes(":not([data-theme])") && env.theme !== "none") {
    return false;
  }
  const densityAttr = selector.match(/\[data-density="([a-z]+)"\]/);
  if (densityAttr && (env.density ?? "comfortable") !== densityAttr[1]) {
    return false;
  }
  const motionAttr = selector.match(/\[data-motion="([a-z]+)"\]/);
  if (motionAttr && (env.motion ?? "system") !== motionAttr[1]) {
    return false;
  }
  return true;
}

/**
 * Effective token map for one environment.
 * `theme`: "light" | "dark" | "auto" | "none" (no data-theme attribute).
 */
export function effectiveTokens(env) {
  const environment = {
    theme: env.theme,
    colorScheme: env.colorScheme ?? (env.theme === "dark" ? "dark" : "light"),
    contrast: env.contrast ?? "normal",
    density: env.density ?? "comfortable",
    motion: env.motion ?? "system",
  };

  const applied = [];
  for (const block of tokenBlocks) {
    if (!block.media.every((query) => mediaMatches(query, environment))) {
      continue;
    }
    const matching = block.selectors.filter((selector) =>
      selectorMatches(selector, environment),
    );
    if (matching.length === 0) {
      continue;
    }
    const weight = Math.max(...matching.map(specificity));
    applied.push({ weight, order: block.order, tokens: block.tokens });
  }

  applied.sort((a, b) => a.weight - b.weight || a.order - b.order);

  const result = new Map();
  for (const entry of applied) {
    for (const [name, value] of entry.tokens) {
      result.set(name, value);
    }
  }
  return result;
}

export const TEXT_PAIRS = [
  ["--text", "--surface", "body on surface"],
  ["--text", "--bg", "body on cloud background"],
  ["--text", "--surface-soft", "body on soft surface"],
  ["--muted", "--surface", "secondary on surface"],
  ["--muted", "--surface-soft", "secondary on soft surface"],
  ["--muted", "--surface-muted", "secondary on muted surface"],
  ["--muted", "--bg", "secondary on background"],
  ["--subtle", "--surface", "tertiary on surface"],
  ["--subtle", "--surface", "topbar separator (decorative glyph)"],
  ["--on-primary", "--primary", "primary button label"],
  ["--info", "--surface", "info text on surface"],
  ["--info", "--info-soft", "info text on info tint"],
  ["--ok", "--surface", "success text on surface"],
  ["--ok", "--ok-soft", "success text on success tint"],
  ["--warn", "--surface", "warning text on surface"],
  ["--warn", "--warn-soft", "warning text on warning tint"],
  ["--danger", "--surface", "error text on surface"],
  ["--danger", "--danger-soft", "error text on error tint"],
];

export const NON_TEXT_PAIRS = [
  ["--focus", "--surface", "focus ring on surface"],
  ["--focus", "--surface-soft", "focus ring on soft surface"],
  ["--control-border", "--surface", "control border on surface"],
  ["--control-border", "--bg", "control border on background"],
];

export function luminance(hexValue) {
  const value = hexValue.replace("#", "");
  const channels = [0, 2, 4].map(
    (offset) => parseInt(value.slice(offset, offset + 2), 16) / 255,
  );
  const linear = channels.map((channel) =>
    channel <= 0.03928
      ? channel / 12.92
      : Math.pow((channel + 0.055) / 1.055, 2.4),
  );
  return 0.2126 * linear[0] + 0.7152 * linear[1] + 0.0722 * linear[2];
}

export function contrast(foreground, background) {
  const a = luminance(foreground);
  const b = luminance(background);
  const high = Math.max(a, b);
  const low = Math.min(a, b);
  return (high + 0.05) / (low + 0.05);
}

export function tokenHex(tokens, name) {
  const value = tokens.get(name);
  if (!value) {
    throw new Error(`missing token ${name}`);
  }
  if (!/^#[0-9a-f]{6}$/i.test(value)) {
    throw new Error(`${name} is not a 6-digit hex: ${value}`);
  }
  return value;
}

/** Every environment the shell can be in, including follow-system variants. */
export const ENVIRONMENTS = [
  { name: "light", theme: "light", colorScheme: "light", contrast: "normal" },
  { name: "light+contrast", theme: "light", colorScheme: "light", contrast: "more" },
  { name: "dark", theme: "dark", colorScheme: "dark", contrast: "normal" },
  { name: "dark+contrast", theme: "dark", colorScheme: "dark", contrast: "more" },
  { name: "auto+system-light", theme: "auto", colorScheme: "light", contrast: "normal" },
  { name: "auto+system-dark", theme: "auto", colorScheme: "dark", contrast: "normal" },
  { name: "auto+system-light+contrast", theme: "auto", colorScheme: "light", contrast: "more" },
  { name: "auto+system-dark+contrast", theme: "auto", colorScheme: "dark", contrast: "more" },
  { name: "none+system-light", theme: "none", colorScheme: "light", contrast: "normal" },
  { name: "none+system-dark+contrast", theme: "none", colorScheme: "dark", contrast: "more" },
];
