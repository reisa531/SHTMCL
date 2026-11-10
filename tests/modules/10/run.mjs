// Module 10 test entry.
// Run: node tests/modules/10/run.mjs     (exit 0 = all assertions passed)
//
// Uses `node --test` with a fixed file list and an explicit cwd so it does not
// depend on directory globbing (same reason as tests/modules/11/run.mjs).

import { spawnSync } from "node:child_process";
import path from "node:path";
import { fileURLToPath } from "node:url";

const here = path.dirname(fileURLToPath(import.meta.url));
const repoRoot = path.resolve(here, "../../..");

const files = [
  "render.test.mjs",
  "game-install.test.mjs",
  "mod-metadata.test.mjs",
  "mod-toggle.test.mjs",
  "game-launch.test.mjs",
  "product-flow.test.mjs",
  "pcl-navigation.test.mjs",
  "download-presentation.test.mjs",
  "history.test.mjs",
  "environment.test.mjs",
  "event-stream.test.mjs",
  "memo.test.mjs",
  "styles.test.mjs",
  "contrast.test.mjs",
].map((name) => path.join(here, name));

const result = spawnSync(process.execPath, ["--test", ...files], {
  cwd: repoRoot,
  stdio: "inherit",
});

if (result.error) {
  console.error(result.error);
  process.exit(1);
}
process.exit(result.status ?? 1);
