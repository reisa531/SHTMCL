import test from "node:test";
import assert from "node:assert/strict";
import { loadBundle } from "./harness.mjs";
const ui = await loadBundle();
const project = { provider: "modrinth", projectId: "sodium", slug: "sodium", title: "Sodium", description: "Render faster", clientSide: "required" };
const version = { versionId: "v1", name: "Release", versionNumber: "non-semver+mc1.20.4", kind: "release", published: "2026-10-30",
  files: [{ name: "sodium.jar", bytes: 1024, primary: true }], dependencies: [{ kind: "required", projectId: "fabric-api", versionId: "fixed-version" }] };
const page = { projectId: "sodium", instanceId: "school", offset: 0, total: 1, target: { gameVersion: "1.20.4", loader: "fabric" }, versions: [version] };
const completed = result => ({ type: "completed", result });

test("Modrinth reads carry project and instance identity, never caller loader or download URLs", async () => {
  const calls = [];
  const api = ui.modrinthApi(async r => { calls.push(r); return completed(r.payload.command.endsWith("project") ? project : page); });
  assert.equal((await api.project("sodium")).title, "Sodium");
  const result = await api.versions("sodium", "school");
  assert.deepEqual(calls.map(r => r.payload), [{command:"content.modrinth.project",projectId:"sodium"}, {command:"content.modrinth.versions",projectId:"sodium",instanceId:"school",offset:0}]);
  assert.equal(calls[1].operationId, undefined);
  assert.equal(result.versions[0].dependencies[0].versionId, "fixed-version");
  assert.equal(result.versions[0].versionNumber, "non-semver+mc1.20.4");
});

test("Modrinth detail parser rejects wrong projects and malformed text", async () => {
  assert.equal((await ui.modrinthApi(async () => completed({...project,slug:"mc1.16.5-sodium"})).project("sodium")).slug,"mc1.16.5-sodium");
  for (const bad of [{...project, projectId:"other"}, {...project, provider:"local"}, {...project, slug:"../x"}, {...project, clientSide:"yes"}, {...project, description:42}]) {
    await assert.rejects(() => ui.modrinthApi(async () => completed(bad)).project("sodium"));
  }
});

test("Modrinth pages reject stale identities, invalid counts, dependencies and files", async () => {
  for (const bad of [{...page, instanceId:"other"}, {...page, offset:20}, {...page, total:0}, {...page, total:-1}, {...page, versions:[version,version]},
    {...page, target:{gameVersion:"1.20.4",loader:"mystery"}},
    {...page, versions:[{...version, dependencies:[{kind:"required",projectId:"../secret"}]}]},
    {...page, versions:[{...version, files:[{name:"mod.jar",bytes:-1,primary:true}]}]},
    {...page, versions:[{...version, kind:"unknown"}]}]) {
    await assert.rejects(() => ui.modrinthApi(async () => completed(bad)).versions("sodium","school"));
  }
});

test("Modrinth genuine empty versions remain empty; network errors preserve retry meaning", async () => {
  const result = await ui.modrinthApi(async () => completed({...page, versions:[], total:0})).versions("sodium","school");
  assert.deepEqual(result.versions, []);
  const failure = {code:"content.modrinth.rate_limited"};
  await assert.rejects(() => ui.modrinthApi(async () => {throw failure;}).project("sodium"), e => e === failure);
  assert.match(ui.resourceSearchError(failure).message,/稍后/);
  assert.equal(ui.resourceSearchError({code:"content.modrinth.not_found"}).retryable,false);
});

test("resource search rejects provider crossover and echoes the nested compatibility target", async () => {
  let sent;
  const api = ui.resourceCatalogApi(async r => { sent=r; return completed({queryId:r.payload.queryId,connected:true,projects:[{...project,provider:"curseforge"}]}); });
  await assert.rejects(() => api.search("modrinth","mods","Sodium",page.target));
  assert.deepEqual(sent.payload.target,page.target);
});
