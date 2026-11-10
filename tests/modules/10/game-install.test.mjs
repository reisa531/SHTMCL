import test from "node:test";
import assert from "node:assert/strict";
import { loadBundle } from "./harness.mjs";
const { gameInstallApi, installError, instanceSettingsApi, instanceSettingsError } = await loadBundle();

test("version queries keep family, search, offset and bounded page size", async () => {
  const requests = [];
  const api = gameInstallApi(async (request) => {
    requests.push(request);
    return { type: "completed", result: { total: 41, entries: [{ id: "c0.0.11a", type: "old_alpha" }] } };
  });
  const page = await api.versions("old_alpha", "c0", 40);
  assert.equal(page.total, 41);
  assert.equal(page.entries[0].id, "c0.0.11a");
  assert.deepEqual(requests[0].payload, { command: "launch.versions.list", kind: "old_alpha", query: "c0", offset: 40, limit: 40 });
  assert.equal(requests[0].operationId, undefined);
});

test("install uses distinct request and operation IDs and preserves a Unicode name", async () => {
  const requests = [];
  const api = gameInstallApi(undefined, async (request) => {
    requests.push(request);
    return { type: "accepted", taskId: "task-real", operationId: request.operationId };
  });
  assert.equal(await api.install("1.7.10", "game-test", " 校园生存 "), "task-real");
  await api.install("1.7.10", "game-test", "校园生存");
  assert.deepEqual(requests[0].payload, { command: "launch.install", versionId: "1.7.10", instanceId: "game-test", name: "校园生存" });
  assert.notEqual(requests[0].requestId, requests[0].operationId);
  assert.notEqual(requests[0].operationId, requests[1].operationId);
});

test("a malformed core response does not become an empty catalogue or installed list", async () => {
  for (const result of [{}, { entries: [], total: -1 }, { entries: [{ id: "x", type: 42 }], total: 1 }]) {
    const api = gameInstallApi(async () => ({ type: "completed", result }));
    await assert.rejects(api.versions("release", "", 0));
    await assert.rejects(api.instances());
  }
  const api = gameInstallApi(undefined, async () => ({ type: "completed", result: { installed: true } }));
  await assert.rejects(api.install("1.7.10", "id", "name"));
});

test("all versions omits kind and preserves future manifest classifications", async () => {
  let payload;
  const api = gameInstallApi(async request => {
    payload = request.payload;
    return { type: "completed", result: { entries: [{ id: "future", type: "future_kind" }], total: 1 } };
  });
  assert.equal((await api.versions("all", "", 0)).entries[0].type, "future_kind");
  assert.deepEqual(payload, { command: "launch.versions.list", query: "", offset: 0, limit: 40 });
});

test("latest metadata comes only from authoritative nonempty response IDs", async () => {
  const api = gameInstallApi(async () => ({ type: "completed", result: { entries: [], total: 0, latestRelease: "1.20.4", latestSnapshot: null } }));
  const page = await api.versions("all", "", 40);
  assert.equal(page.latestRelease, "1.20.4");
  assert.equal(Object.hasOwn(page, "latestSnapshot"), false);
});

test("instance names and versions come from the persisted core index", async () => {
  const expected = { id: "abc", name: "校园生存", version: { gameVersion: "1.7.10", loader: "vanilla" } };
  const api = gameInstallApi(async () => ({ type: "completed", result: { instances: [expected] } }));
  assert.deepEqual(await api.instances(), [expected]);
  assert.match(installError({ code: "launch.install.busy" }), /另一个实例/);
  assert.equal(installError(new Error("raw stack")), "未能开始创建，请重试。");
});

test("instance presentation preserves known loader and pack fields and rejects invalid counts", async () => {
  const source = {id:"pack",name:"Pack",version:{gameVersion:"1.20.1",loader:"fabric",loaderVersion:"0.16.9"},presentation:{modFileCount:12,modpackName:"My Pack"}};
  const api = gameInstallApi(async () => ({type:"completed",result:{instances:[source]}}));
  assert.deepEqual(await api.instances(), [source]);
  source.presentation.modFileCount = -1;
  assert.equal((await api.instances())[0].presentation.modFileCount, undefined);
});

test("instance settings preserve authoritative optional snapshot fields", async () => {
  const api = gameInstallApi(async () => ({type:"completed",result:{instances:[{
    id:"school",name:"School",javaMajor:17,identityId:"player-1",profileId:"profile-1",
    version:{gameVersion:"1.20.4",loader:"fabric"}
  }]}}));
  assert.deepEqual(await api.instances(), [{
    id:"school",name:"School",javaMajor:17,identityId:"player-1",profileId:"profile-1",
    version:{gameVersion:"1.20.4",loader:"fabric"}
  }]);
});

test("invalid optional instance settings are omitted instead of guessed", async () => {
  const api = gameInstallApi(async () => ({type:"completed",result:{instances:[{
    id:"school",name:"School",javaMajor:0,identityId:"  ",profileId:42,
    version:{gameVersion:"1.20.4",loader:"fabric"}
  }]}}));
  assert.deepEqual(await api.instances(), [{id:"school",name:"School",version:{gameVersion:"1.20.4",loader:"fabric"}}]);
});

test("rename uses the exact snapshot revision and only patches the name", async () => {
  const requests = [];
  const api = instanceSettingsApi(async request => {
    requests.push(request);
    return {type:"completed",result:{instance:{id:"school",name:"School",revision:7,state:"ready"}}};
  }, async request => {
    requests.push(request);
    return {type:"completed",result:{instance:{id:"school",name:request.payload.patch.name,revision:8,state:"ready"}}};
  });
  const value = await api.read("school");
  await api.rename(value.id, value.revision, " 校园生存 ");
  assert.deepEqual(requests[0].payload, {command:"instances.snapshot",instanceId:"school"});
  assert.equal(requests[0].operationId, undefined);
  assert.equal(requests[1].expectedRevision, 7);
  assert.notEqual(requests[1].requestId, requests[1].operationId);
  assert.deepEqual(requests[1].payload, {command:"instances.update",instanceId:"school",patch:{name:"校园生存"}});
});

test("rename rejects malformed reads, invalid drafts and mismatched write acknowledgements", async () => {
  for (const instance of [null, {id:"other",name:"X",revision:1,state:"ready"}, {id:"school",name:"X",revision:0,state:"ready"}]) {
    await assert.rejects(instanceSettingsApi(async () => ({type:"completed",result:{instance}})).read("school"));
  }
  let calls = 0;
  const api = instanceSettingsApi(undefined, async () => {
    calls++;
    return {type:"completed",result:{instance:{id:"school",name:"wrong",revision:2,state:"ready"}}};
  });
  for (const name of [" ", "bad\nname", "x".repeat(201)]) await assert.rejects(api.rename("school",1,name));
  await assert.rejects(api.rename("school",0,"Name"));
  assert.equal(calls,0);
  await assert.rejects(api.rename("school",1,"Name"));
  assert.equal(calls,1);
});

test("rename preserves core conflict and lease failures without automatic retry", async () => {
  let calls = 0;
  for (const code of ["instances.revision.stale", "instances.instance.running", "instances.instance.locked"]) {
    const error = {code};
    const api = instanceSettingsApi(undefined, async () => { calls++; throw error; });
    await assert.rejects(api.rename("school",1,"Name"), value => value === error);
    assert.doesNotMatch(instanceSettingsError(error), /未能确认/);
  }
  assert.equal(calls,3);
  assert.doesNotMatch(instanceSettingsError(new Error("private stack")), /private stack/);
});
