import test from "node:test";
import assert from "node:assert/strict";
import { createHash } from "node:crypto";
import { mkdtemp, mkdir, readFile, writeFile, rm } from "node:fs/promises";
import { writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { resolve } from "node:path";
import { generateAur } from "../../scripts/generate-aur.mjs";
import { packageArtifacts } from "../../scripts/package-artifacts.mjs";
import { repoRoot } from "../../scripts/release-version.mjs";

const sha = data => createHash("sha256").update(data).digest("hex");
async function put(root, name, data) {
  const path = resolve(root, name); await mkdir(resolve(path, ".."), { recursive: true });
  await writeFile(path, data); return path;
}
async function fixture(t, version = "0.1.0") {
  const root = await mkdtemp(resolve(tmpdir(), "shtmcl-aur-test-"));
  t.after(() => rm(root, { recursive: true, force: true }));
  await put(root, "Cargo.toml", `[workspace.package]\nversion = "${version}"\n`);
  for (const name of ["package.json", "apps/desktop/package.json", "apps/desktop/src-tauri/tauri.conf.json"]) {
    await put(root, name, JSON.stringify({ version, license: "MIT" }));
  }
  await put(root, "LICENSE", "synthetic license fixture");
  await put(root, "packaging/aur/PKGBUILD.in", await readFile(resolve(repoRoot, "packaging/aur/PKGBUILD.in")));
  const bytes = Buffer.from("synthetic deb archive");
  const name = `SHTMCL_${version}_linux_x86_64.deb`;
  await put(root, `aur-input/${name}`, bytes);
  await put(root, "aur-input/build-linux-x86_64.json", JSON.stringify({ version, platform: "linux", arch: "x86_64", artifacts: [{ name, sha256: sha(bytes) }] }));
  return root;
}

test("AUR generation pins the deb and license and delegates SRCINFO to makepkg", async t => {
  const root = await fixture(t); let invocation;
  const result = await generateAur({ root, repository: "example/shtmcl", ref: "refs/tags/v0.1.0", run(command, args, options) {
    invocation = { command, args, options }; return "pkgbase = shtmcl-bin\npkgname = shtmcl-bin\n";
  } });
  const text = await readFile(resolve(result.output, "PKGBUILD"), "utf8");
  assert.equal(invocation.command, "makepkg"); assert.deepEqual(invocation.args, ["--printsrcinfo"]);
  assert.equal(invocation.options.cwd, result.output);
  assert.ok(text.includes("https://github.com/example/shtmcl"));
  assert.ok(text.includes(result.sha256));
  assert.ok(text.includes(sha("synthetic license fixture")));
  assert.ok(text.includes("license=('MIT')"));
  assert.ok(!text.includes("SKIP"));
  assert.equal(await readFile(resolve(result.output, ".SRCINFO"), "utf8"), "pkgbase = shtmcl-bin\npkgname = shtmcl-bin\n");
  assert.equal(await readFile(resolve(result.output, "shtmcl-bin-0.1.0-x86_64.deb"), "utf8"), "synthetic deb archive");
});

test("AUR generation rejects a changed deb before any recipe is generated", async t => {
  const root = await fixture(t);
  await put(root, "aur-input/SHTMCL_0.1.0_linux_x86_64.deb", "changed bytes");
  await assert.rejects(() => generateAur({ root, repository: "example/shtmcl", ref: "" }), /checksum/);
  await assert.rejects(() => readFile(resolve(root, "target/aur/shtmcl-bin/PKGBUILD")), { code: "ENOENT" });
});

test("AUR repository and stable-version rules fail before invoking the shell", async t => {
  const root = await fixture(t, "0.1.0-beta.1");
  for (const repository of ["", "../repo", "example/repo;echo-injected", "https://github.com/example/shtmcl"]) {
    await assert.rejects(() => generateAur({ root, repository, ref: "" }), /GITHUB_REPOSITORY/);
  }
  await assert.rejects(() => generateAur({ root, repository: "example/shtmcl", ref: "" }), /stable/);
});

test("AUR generator cannot silently reuse a package with the wrong version or license", async t => {
  const root = await fixture(t);
  const path = resolve(root, "aur-input/build-linux-x86_64.json");
  const original = JSON.parse(await readFile(path, "utf8"));
  await writeFile(path, JSON.stringify({ ...original, version: "9.9.9" }));
  await assert.rejects(() => generateAur({ root, repository: "example/shtmcl", ref: "" }), /matching Linux/);
  await writeFile(path, JSON.stringify(original));
  await put(root, "package.json", JSON.stringify({ version: "0.1.0", license: "other" }));
  await assert.rejects(() => generateAur({ root, repository: "example/shtmcl", ref: "" }), /license/);
});

test("AUR source archive explicitly includes only PKGBUILD, SRCINFO and LICENSE", async t => {
  const root = await fixture(t);
  const recipe = "target/aur/shtmcl-bin";
  for (const file of ["PKGBUILD", ".SRCINFO", "LICENSE", "shtmcl-bin-0.1.0-1-x86_64.pkg.tar.zst", "cached.deb", "namcap.log"]) await put(root, `${recipe}/${file}`, file);
  let invocation;
  const manifest = await packageArtifacts("aur", { root, env: {}, run(command, args) {
    invocation = { command, args }; writeFileSync(args[1], "synthetic source archive");
  } });
  assert.equal(invocation.command, "tar");
  assert.deepEqual(invocation.args.slice(-3), ["PKGBUILD", ".SRCINFO", "LICENSE"]);
  assert.deepEqual(manifest.artifacts.map(a => a.name), ["shtmcl-bin-0.1.0-1-x86_64.pkg.tar.zst", "SHTMCL_0.1.0_aur_x86_64.recipe.tar.gz"]);
  assert.ok(!(await readFile(resolve(root, "release-assets/SHA256SUMS-aur-x86_64.txt"), "utf8")).includes("cached.deb"));
});
