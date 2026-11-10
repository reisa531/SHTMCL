// Minimal cascade evaluator for the shell stylesheet.
//
// It answers one question: "what box height does this element get in this
// environment?" — enough to assert the touch-target tokens without a browser.
// Supported selector shapes are exactly the ones the stylesheet uses for
// controls: a single compound selector, or `:root[...] <compound>`. Anything
// else is reported as unsupported instead of being silently ignored, so a new
// rule shape cannot pretend to be covered.

import { css, effectiveTokens } from "./tokens.mjs";

function stripComments(source) {
  return source.replace(/\/\*[\s\S]*?\*\//g, "");
}

function parseDeclarations(body) {
  const declarations = new Map();
  for (const match of body.matchAll(/([a-z-]+)\s*:\s*([^;]+);/g)) {
    declarations.set(match[1], match[2].trim());
  }
  return declarations;
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
      out.push({
        selectors: prelude
          .split(",")
          .map((part) => part.trim())
          .filter(Boolean),
        media: mediaStack.slice(),
        declarations: parseDeclarations(body),
        order: out.length,
      });
    }
    index = cursor + 1;
  }
}

export const RULES = (() => {
  const rules = [];
  walk(stripComments(css), [], rules);
  return rules;
})();

export const unsupportedSelectors = new Set();

function splitCompound(selector) {
  return selector.match(/:not\([^)]*\)|::?[a-z-]+(?:\([^)]*\))?|\.[A-Za-z0-9_-]+|\[[^\]]+\]|[A-Za-z][A-Za-z0-9-]*/g) ?? [];
}

function matchesRoot(selector, rootAttributes) {
  const attrs = [...selector.matchAll(/\[([a-z-]+)="([a-z-]+)"\]/g)];
  if (attrs.length === 0) {
    return selector === ":root";
  }
  return attrs.every(([, name, value]) => rootAttributes[name] === value);
}

function matchesCompound(selector, element) {
  for (const part of splitCompound(selector)) {
    if (part.startsWith(":not(")) {
      const inner = part.slice(5, -1).trim();
      if (matchesCompound(inner, element)) {
        return false;
      }
      continue;
    }
    if (part.startsWith(".")) {
      if (!element.classes.includes(part.slice(1))) {
        return false;
      }
      continue;
    }
    if (part.startsWith("[")) {
      continue; // attribute selectors on the element are not used here
    }
    if (part.startsWith(":")) {
      continue; // pseudo-classes such as :hover cannot be asserted statically
    }
    // Type selector.
    if (element.tag !== part) {
      return false;
    }
  }
  return true;
}

function mediaMatches(query, env) {
  const text = query.toLowerCase();
  if (text.includes("prefers-color-scheme: dark")) return env.colorScheme === "dark";
  if (text.includes("prefers-color-scheme: light")) return env.colorScheme === "light";
  if (text.includes("prefers-contrast: more")) return env.contrast === "more";
  if (text.includes("prefers-reduced-motion")) return false;
  if (text.includes("forced-colors")) return false;
  return false;
}

function selectorMatches(selector, element, rootAttributes, env) {
  const parts = selector.split(/\s+/).filter(Boolean);
  if (parts.length === 1) {
    return matchesCompound(parts[0], element);
  }
  if (parts.length === 2 && parts[0].startsWith(":root")) {
    return matchesRoot(parts[0], rootAttributes) && matchesCompound(parts[1], element);
  }
  unsupportedSelectors.add(selector);
  return false;
}

function specificity(selector) {
  const parts = splitCompound(selector);
  let weight = 0;
  for (const part of parts) {
    if (part.startsWith(":not(")) {
      weight += 1 + specificity(part.slice(5, -1).trim());
    } else if (part.startsWith(".") || part.startsWith("[") || part.startsWith(":")) {
      weight += 1;
    }
  }
  return weight;
}

/** Ordered declarations applied to `element` in `env` (low → high priority). */
export function declarationsFor(element, env) {
  const environment = {
    theme: env.theme ?? "light",
    colorScheme: env.colorScheme ?? "light",
    contrast: env.contrast ?? "normal",
    density: env.density ?? "comfortable",
    motion: env.motion ?? "system",
  };
  const rootAttributes = {
    "data-theme": environment.theme,
    "data-density": environment.density,
    "data-motion": environment.motion,
  };

  const applied = [];
  for (const rule of RULES) {
    if (!rule.media.every((query) => mediaMatches(query, environment))) {
      continue;
    }
    const matching = rule.selectors.filter((selector) =>
      selectorMatches(selector, element, rootAttributes, environment),
    );
    if (matching.length === 0) {
      continue;
    }
    applied.push({
      weight: Math.max(...matching.map(specificity)),
      order: rule.order,
      declarations: rule.declarations,
      selector: matching.join(", "),
    });
  }
  applied.sort((a, b) => a.weight - b.weight || a.order - b.order);
  return applied;
}

function resolveLength(value, env, depth = 0) {
  if (value === undefined || depth > 8) {
    return null;
  }
  const tokens = effectiveTokens(env);
  const variable = value.match(/^var\((--[a-z0-9-]+)(?:,\s*(.+))?\)$/);
  if (variable) {
    const resolved = tokens.get(variable[1]);
    if (resolved === undefined) {
      return variable[2] ? resolveLength(variable[2], env, depth + 1) : null;
    }
    return resolveLength(resolved, env, depth + 1);
  }
  const pixels = value.match(/^(\d+(?:\.\d+)?)px$/);
  return pixels ? Number(pixels[1]) : null;
}

/**
 * Rendered box height of `element` in `env`: `max(height, min-height)`, or null
 * when neither declaration resolves (the caller must then fail loudly).
 */
export function boxHeight(element, env) {
  const applied = declarationsFor(element, env);
  const height = new Map();
  const minHeight = new Map();
  for (const rule of applied) {
    if (rule.declarations.has("height")) {
      height.set(rule.selector, rule.declarations.get("height"));
    }
    if (rule.declarations.has("min-height")) {
      minHeight.set(rule.selector, rule.declarations.get("min-height"));
    }
  }
  const heightValue = [...height.values()].at(-1);
  const minHeightValue = [...minHeight.values()].at(-1);

  const resolvedHeight = resolveLength(heightValue, env);
  const resolvedMin = resolveLength(minHeightValue, env);
  if (resolvedHeight === null && resolvedMin === null) {
    return { value: null, heightValue, minHeightValue };
  }
  return {
    value: Math.max(resolvedHeight ?? 0, resolvedMin ?? 0),
    heightValue,
    minHeightValue,
  };
}

export const ELEMENTS = {
  primaryButton: { tag: "button", classes: ["btn", "btn--primary", "btn--md"] },
  secondaryButton: { tag: "button", classes: ["btn", "btn--secondary", "btn--md"] },
  smallButton: { tag: "button", classes: ["btn", "btn--secondary", "btn--sm"] },
  iconButton: { tag: "button", classes: ["icon-btn"] },
  tab: { tag: "button", classes: ["tab"] },
  listRow: { tag: "li", classes: ["list__row"] },
  navItem: { tag: "button", classes: ["nav__item"] },
  segment: { tag: "label", classes: ["segment"] },
};
