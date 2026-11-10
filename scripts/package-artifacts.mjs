import { createHash } from "node:crypto";
import { execFileSync } from "node:child_process";
import { createReadStream } from "node:fs";
import { copyFile, mkdir, readdir, stat, writeFile, appendFile } from "node:fs/promises";
import { basename, resolve } from "node:path";
import { pathToFileURL } from "node:url";
import { releaseVersion, repoRoot } from "./release-version.mjs";

const targets = {
  windows: { triple: "x86_64-pc-windows-msvc", arch: "x86_64" },
  linux: { triple: "x86_64-unknown-linux-gnu", arch: "x86_64" },
  macos: { triple: "universal-apple-darwin", arch: "universal" },
  aur: { triple: "x86_64-unknown-linux-gnu", arch: "x86_64" },
};

async function onlyFile(directory, extension) {
  const entries = (await readdir(directory, { withFileTypes: true }))
    .filter(entry => entry.isFile() && entry.name.endsWith(extension));
  if (entries.length !== 1) throw new Error(`Expected one ${extension} in ${directory}, found ${entries.length}`);
  return resolve(directory, entries[0].name);
}
async function digest(file) {
  const hash = createHash("sha256");
  for await (const chunk of createReadStream(file)) hash.update(chunk);
  return hash.digest("hex");
}

/** Collect a fresh runner's native output, preserving macOS app permissions in a ZIP. */
export async function packageArtifacts(platform, { root = repoRoot, output = resolve(root, "release-assets"),
  env = process.env, run = execFileSync } = {}) {
  const target = targets[platform];
  if (!target) throw new Error(`Unsupported release platform: ${platform}`);
  const version = releaseVersion(root, env.GITHUB_REF ?? "");
  const id = `${platform}-${target.arch}`;
  const prefix = `SHTMCL_${version}_${platform}_${target.arch}`;
  const bundle = resolve(root, "target", target.triple, "release", "bundle");
  const files = [];
  let app;
  let aurRecipe;
  if (platform === "windows") {
    const exe = resolve(root, "target/portable/SHTMCL.exe");
    const bytes = (await stat(exe)).size;
    if (bytes > 40 * 1024 ** 2) throw new Error(`Windows EXE exceeds 40 MiB: ${bytes} bytes`);
    if (bytes > 25 * 1024 ** 2) console.warn(`Windows EXE exceeds the 25 MiB target: ${bytes} bytes`);
    files.push({ source: exe, name: "SHTMCL.exe" });
  } else if (platform === "linux") {
    files.push({ source: await onlyFile(resolve(bundle, "appimage"), ".AppImage"), name: `${prefix}.AppImage` });
    files.push({ source: await onlyFile(resolve(bundle, "deb"), ".deb"), name: `${prefix}.deb` });
  } else if (platform === "aur") {
    aurRecipe = resolve(root, "target/aur/shtmcl-bin");
    for (const name of ["PKGBUILD", ".SRCINFO", "LICENSE"]) {
      if (!(await stat(resolve(aurRecipe, name))).isFile()) throw new Error(`Missing AUR source: ${name}`);
    }
    const source = await onlyFile(aurRecipe, ".pkg.tar.zst");
    files.push({ source, name: basename(source) });
  } else {
    app = resolve(bundle, "macos/SHTMCL.app");
    if (!(await stat(app)).isDirectory()) throw new Error(`Missing app bundle: ${app}`);
    files.push({ source: await onlyFile(resolve(bundle, "dmg"), ".dmg"), name: `${prefix}.dmg` });
  }
  // Reusing an output directory could mix versions. CI always starts with an empty one.
  await mkdir(output, { recursive: true });
  if ((await readdir(output)).length) throw new Error(`Release output must be empty: ${output}`);
  for (const file of files) await copyFile(file.source, resolve(output, file.name));
  if (app) {
    const name = `${prefix}.app.zip`;
    run("ditto", ["-c", "-k", "--sequesterRsrc", "--keepParent", app, resolve(output, name)]);
    files.push({ name });
  }
  if (aurRecipe) {
    const name = `${prefix}.recipe.tar.gz`;
    run("tar", ["-czf", resolve(output, name), "-C", aurRecipe, "PKGBUILD", ".SRCINFO", "LICENSE"]);
    files.push({ name });
  }
  const artifacts = [];
  for (const file of files) {
    const path = resolve(output, file.name);
    artifacts.push({ name: file.name, bytes: (await stat(path)).size, sha256: await digest(path) });
  }
  const manifest = { version, platform, arch: target.arch, target: target.triple,
    commit: env.GITHUB_SHA ?? null, ref: env.GITHUB_REF ?? null, variant: "standard",
    signing: platform === "macos" ? "ad-hoc" : "unsigned", artifacts };
  const manifestName = `build-${id}.json`;
  await writeFile(resolve(output, manifestName), `${JSON.stringify(manifest, null, 2)}\n`);
  const sums = artifacts.map(file => `${file.sha256}  ${file.name}`);
  sums.push(`${await digest(resolve(output, manifestName))}  ${manifestName}`);
  await writeFile(resolve(output, `SHA256SUMS-${id}.txt`), `${sums.join("\n")}\n`);
  if (env.GITHUB_STEP_SUMMARY) {
    await appendFile(env.GITHUB_STEP_SUMMARY, `### ${platform} ${target.arch} · ${version}\n\n| Artifact | MiB |\n| --- | ---: |\n${artifacts.map(file => `| ${file.name} | ${(file.bytes / 1024 ** 2).toFixed(2)} |`).join("\n")}\n\nCommit: \`${manifest.commit}\`\n\n`);
  }
  return manifest;
}

if (process.argv[1] && import.meta.url === pathToFileURL(resolve(process.argv[1])).href) {
  const manifest = await packageArtifacts(process.argv[2]);
  console.log(`${manifest.version}: ${manifest.artifacts.map(file => basename(file.name)).join(", ")}`);
}
