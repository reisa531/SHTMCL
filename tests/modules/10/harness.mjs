// Bundles `ssr-entry.tsx` with the esbuild copy that ships next to vite, then
// imports it. There is no test runner dependency in this workspace, so the
// bundle step is done here instead of through vite/vitest.

import { existsSync, mkdirSync, readdirSync, rmSync } from "node:fs";
import path from "node:path";
import { fileURLToPath, pathToFileURL } from "node:url";

const here = path.dirname(fileURLToPath(import.meta.url));
export const repoRoot = path.resolve(here, "../../..");
const desktopRoot = path.join(repoRoot, "apps", "desktop");

export function findEsbuild() {
  const local = path.join(desktopRoot, "node_modules", "esbuild", "lib", "main.js");
  if (existsSync(local)) {
    return local;
  }
  const store = path.join(repoRoot, "node_modules", ".pnpm");
  if (existsSync(store)) {
    const candidates = readdirSync(store)
      .filter((entry) => entry.startsWith("esbuild@"))
      .sort();
    for (const entry of candidates) {
      const candidate = path.join(store, entry, "node_modules", "esbuild", "lib", "main.js");
      if (existsSync(candidate)) {
        return candidate;
      }
    }
  }
  return null;
}

let cached = null;

export async function loadBundle() {
  if (cached) {
    return cached;
  }

  const esbuildMain = findEsbuild();
  if (!esbuildMain) {
    throw new Error(
      "esbuild not found: expected apps/desktop/node_modules/esbuild or node_modules/.pnpm/esbuild@*/node_modules/esbuild",
    );
  }

  const imported = await import(pathToFileURL(esbuildMain).href);
  const esbuild = imported.default ?? imported;

  const outDir = path.join(here, ".build");
  mkdirSync(outDir, { recursive: true });
  const outfile = path.join(outDir, `ssr-entry-${process.pid}.mjs`);

  await esbuild.build({
    entryPoints: [path.join(here, "ssr-entry.tsx")],
    absWorkingDir: desktopRoot,
    nodePaths: [path.join(desktopRoot, "node_modules")],
    bundle: true,
    format: "esm",
    platform: "node",
    target: "node18",
    jsx: "automatic",
    external: ["@tauri-apps/api/core", "@tauri-apps/api/*"],
    // The bundled React server renderer is CommonJS and calls `require` for
    // node builtins; ESM output needs a real `require` in module scope.
    banner: {
      js: 'import { createRequire as __createRequire } from "node:module";\nconst require = __createRequire(import.meta.url);',
    },
    logLevel: "silent",
    outfile,
  });

  const module = await import(pathToFileURL(outfile).href);
  cached = module;
  process.once("exit", () => {
    rmSync(outfile, { force: true });
  });
  return module;
}
