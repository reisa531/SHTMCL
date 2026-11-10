import { appendFileSync, readFileSync } from "node:fs";
import { resolve } from "node:path";
import { fileURLToPath, pathToFileURL } from "node:url";

export const repoRoot = fileURLToPath(new URL("../", import.meta.url));

/** One source version shared by every platform; tag builds must match it. */
export function releaseVersion(root = repoRoot, ref = "") {
  const cargo = readFileSync(resolve(root, "Cargo.toml"), "utf8");
  const section = cargo.match(/\[workspace\.package\]([\s\S]*?)(?=\n\[|$)/)?.[1];
  const version = section?.match(/^version\s*=\s*"([^"]+)"/m)?.[1];
  if (!version || !/^\d+\.\d+\.\d+(?:-[\w.-]+)?(?:\+[\w.-]+)?$/.test(version)) {
    throw new Error("Cargo workspace.package.version must be a release version");
  }
  for (const name of ["package.json", "apps/desktop/package.json", "apps/desktop/src-tauri/tauri.conf.json"]) {
    if (JSON.parse(readFileSync(resolve(root, name), "utf8")).version !== version) {
      throw new Error(`${name} version must match Cargo workspace version ${version}`);
    }
  }
  if (ref.startsWith("refs/tags/") && ref !== `refs/tags/v${version}`) {
    throw new Error(`Tag ${ref} must be v${version}; update all four version fields together`);
  }
  return version;
}

if (process.argv[1] && import.meta.url === pathToFileURL(resolve(process.argv[1])).href) {
  const version = releaseVersion(repoRoot, process.env.GITHUB_REF ?? "");
  if (process.env.GITHUB_OUTPUT) {
    appendFileSync(process.env.GITHUB_OUTPUT, `version=${version}\naur-stable=${/^\d+\.\d+\.\d+$/.test(version)}\n`);
  }
  console.log(version);
}
