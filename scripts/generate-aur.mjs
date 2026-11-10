import { createHash } from "node:crypto";
import { execFileSync } from "node:child_process";
import { createReadStream } from "node:fs";
import { copyFile, mkdir, readFile, readdir, writeFile } from "node:fs/promises";
import { resolve } from "node:path";
import { pathToFileURL } from "node:url";
import { releaseVersion, repoRoot } from "./release-version.mjs";

async function digest(file) {
  const hash = createHash("sha256");
  for await (const chunk of createReadStream(file)) hash.update(chunk);
  return hash.digest("hex");
}

/** Build an AUR recipe against the exact deb produced by this workflow run.
 * The cached deb makes validation possible before the GitHub release is public.
 * Only PKGBUILD/.SRCINFO/LICENSE are included in the distributable recipe archive.
 */
export async function generateAur({ root = repoRoot, repository = process.env.GITHUB_REPOSITORY,
  input = resolve(root, "aur-input"), output = resolve(root, "target/aur/shtmcl-bin"),
  ref = process.env.GITHUB_REF ?? "", run = execFileSync } = {}) {
  if (!repository || !/^[A-Za-z0-9][A-Za-z0-9-]*\/[A-Za-z0-9_.-]+$/.test(repository)) {
    throw new Error("Set GITHUB_REPOSITORY=owner/repository to generate the real upstream URLs");
  }
  const version = releaseVersion(root, ref);
  if (!/^\d+\.\d+\.\d+$/.test(version)) throw new Error("shtmcl-bin tracks stable x.y.z releases");
  if (JSON.parse(await readFile(resolve(root, "package.json"), "utf8")).license !== "MIT") {
    throw new Error("Review the AUR recipe when changing the project license");
  }
  const manifest = JSON.parse(await readFile(resolve(input, "build-linux-x86_64.json"), "utf8"));
  const name = `SHTMCL_${version}_linux_x86_64.deb`;
  const asset = manifest.artifacts?.find(file => file.name === name);
  if (manifest.version !== version || manifest.platform !== "linux" || manifest.arch !== "x86_64" || !asset) {
    throw new Error("AUR input must be the matching Linux release artifacts");
  }
  const deb = resolve(input, name);
  const sha256 = await digest(deb);
  if (sha256 !== asset.sha256) throw new Error("Linux deb checksum does not match its build manifest");
  const license = resolve(root, "LICENSE");
  const template = await readFile(resolve(root, "packaging/aur/PKGBUILD.in"), "utf8");
  const pkgbuild = template.replaceAll("@VERSION@", version).replaceAll("@REPOSITORY@", repository)
    .replaceAll("@DEB_SHA256@", sha256).replaceAll("@LICENSE_SHA256@", await digest(license));
  await mkdir(output, { recursive: true });
  if ((await readdir(output)).length) throw new Error(`AUR output must be empty: ${output}`);
  await writeFile(resolve(output, "PKGBUILD"), pkgbuild);
  await copyFile(license, resolve(output, "LICENSE"));
  await copyFile(deb, resolve(output, `shtmcl-bin-${version}-x86_64.deb`));
  const srcinfo = run("makepkg", ["--printsrcinfo"], { cwd: output, encoding: "utf8" });
  await writeFile(resolve(output, ".SRCINFO"), srcinfo);
  return { pkgname: "shtmcl-bin", version, repository, sha256, output };
}

if (process.argv[1] && import.meta.url === pathToFileURL(resolve(process.argv[1])).href) {
  const recipe = await generateAur({ input: process.argv[2] ? resolve(process.argv[2]) : undefined });
  console.log(`${recipe.pkgname} ${recipe.version}: ${recipe.output}`);
}
