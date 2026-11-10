import test from "node:test";
import assert from "node:assert/strict";
import { createHash } from "node:crypto";
import { mkdtemp, mkdir, readFile, writeFile, rm, open, readdir } from "node:fs/promises";
import { tmpdir } from "node:os";
import { resolve } from "node:path";
import { releaseVersion } from "../../scripts/release-version.mjs";
import { packageArtifacts } from "../../scripts/package-artifacts.mjs";

async function fixture(t, version = "0.1.0") {
  const root = await mkdtemp(resolve(tmpdir(), "shtmcl-packaging-"));
  t.after(() => rm(root, { recursive: true, force: true }));
  await put(root, "Cargo.toml", `[workspace]\nmembers=[]\n[workspace.package]\nversion = "${version}"\n[profile.release]\nstrip=true\n`);
  for (const path of ["package.json", "apps/desktop/package.json", "apps/desktop/src-tauri/tauri.conf.json"]) await put(root, path, JSON.stringify({ version }));
  return root;
}
async function put(root, path, data) {
  const file = resolve(root, path);
  await mkdir(resolve(file, ".."), { recursive: true });
  await writeFile(file, data);
  return file;
}
const digest = bytes => createHash("sha256").update(bytes).digest("hex");
async function verifySums(directory, name) {
  for (const line of (await readFile(resolve(directory, name), "utf8")).trim().split("\n")) {
    const [hash, file] = line.split("  ");
    assert.equal(digest(await readFile(resolve(directory, file))), hash, file);
  }
}

test("all platforms share four source versions and the exact release tag", async t => {
  const root = await fixture(t, "1.2.3-beta.1");
  assert.equal(releaseVersion(root, "refs/heads/main"), "1.2.3-beta.1");
  assert.equal(releaseVersion(root, "refs/tags/v1.2.3-beta.1"), "1.2.3-beta.1");
  assert.throws(() => releaseVersion(root, "refs/tags/v1.2.3"), /must be v1.2.3-beta.1/);
  await put(root, "package.json", JSON.stringify({ version: "1.2.4" }));
  assert.throws(() => releaseVersion(root), /package.json version must match/);
});

test("a missing workspace version cannot fall back to a different Cargo version", async t => {
  const root = await fixture(t);
  await put(root, "Cargo.toml", '[package]\nversion="9.9.9"\n');
  assert.throws(() => releaseVersion(root), /workspace.package.version/);
});

test("Windows ships one EXE, records commit and hash, and excludes portable user data", async t => {
  const root = await fixture(t);
  const bytes = Buffer.from("synthetic executable bytes");
  await put(root, "target/portable/SHTMCL.exe", bytes);
  await put(root, "target/portable/SHTMCL-data/private.txt", "user data");
  const manifest = await packageArtifacts("windows", { root, env: { GITHUB_REF: "refs/tags/v0.1.0", GITHUB_SHA: "test-commit" } });
  assert.equal(manifest.commit, "test-commit");
  assert.equal(manifest.target, "x86_64-pc-windows-msvc");
  assert.deepEqual(manifest.artifacts, [{ name: "SHTMCL.exe", bytes: bytes.length, sha256: digest(bytes) }]);
  assert.deepEqual((await readdir(resolve(root, "release-assets"))).sort(), ["SHA256SUMS-windows-x86_64.txt", "SHTMCL.exe", "build-windows-x86_64.json"].sort());
  await verifySums(resolve(root, "release-assets"), "SHA256SUMS-windows-x86_64.txt");
  await assert.rejects(() => packageArtifacts("windows", { root, env: {} }), /must be empty/);
});

test("Windows enforces the uncompressed 40 MiB gate before creating release output", async t => {
  const root = await fixture(t);
  const path = await put(root, "target/portable/SHTMCL.exe", "");
  const file = await open(path, "r+");
  await file.truncate(40 * 1024 ** 2 + 1); await file.close();
  await assert.rejects(() => packageArtifacts("windows", { root, env: {} }), /exceeds 40 MiB/);
  await assert.rejects(() => readdir(resolve(root, "release-assets")), { code: "ENOENT" });
});

test("Linux requires both native formats and gives assets unique versioned names", async t => {
  const root = await fixture(t);
  const bundle = "target/x86_64-unknown-linux-gnu/release/bundle";
  await put(root, `${bundle}/appimage/SHTMCL.AppImage`, "synthetic appimage");
  await put(root, `${bundle}/deb/SHTMCL.deb`, "synthetic deb");
  const manifest = await packageArtifacts("linux", { root, env: {} });
  assert.deepEqual(manifest.artifacts.map(v => v.name), ["SHTMCL_0.1.0_linux_x86_64.AppImage", "SHTMCL_0.1.0_linux_x86_64.deb"]);
  await verifySums(resolve(root, "release-assets"), "SHA256SUMS-linux-x86_64.txt");
});

test("ambiguous old Linux bundles fail rather than selecting a random version", async t => {
  const root = await fixture(t);
  const base = "target/x86_64-unknown-linux-gnu/release/bundle";
  await put(root, `${base}/appimage/old.AppImage`, "old");
  await put(root, `${base}/appimage/current.AppImage`, "new");
  await assert.rejects(() => packageArtifacts("linux", { root, env: {} }), /found 2/);
});

test("macOS collection invokes ditto with the app parent and records universal/ad-hoc metadata", async t => {
  const root = await fixture(t);
  const base = "target/universal-apple-darwin/release/bundle";
  await mkdir(resolve(root, base, "macos/SHTMCL.app/Contents/MacOS"), { recursive: true });
  await put(root, `${base}/dmg/SHTMCL.dmg`, "synthetic dmg");
  let invocation;
  // Only the archive invocation is mocked; this does not test native macOS signing.
  const { writeFileSync } = await import("node:fs");
  const manifest = await packageArtifacts("macos", { root, env: {}, run(command, args) {
    invocation = { command, args };
    writeFileSync(args.at(-1), "synthetic app archive");
  } });
  assert.equal(invocation.command, "ditto");
  assert.deepEqual(invocation.args.slice(0, 4), ["-c", "-k", "--sequesterRsrc", "--keepParent"]);
  assert.equal(manifest.arch, "universal");
  assert.equal(manifest.signing, "ad-hoc");
  assert.deepEqual(manifest.artifacts.map(v => v.name), ["SHTMCL_0.1.0_macos_universal.dmg", "SHTMCL_0.1.0_macos_universal.app.zip"]);
  await verifySums(resolve(root, "release-assets"), "SHA256SUMS-macos-universal.txt");
});

test("unsupported release platforms fail explicitly", async t => {
  const root = await fixture(t);
  await assert.rejects(() => packageArtifacts("unknown", { root, env: {} }), /Unsupported release platform/);
});
