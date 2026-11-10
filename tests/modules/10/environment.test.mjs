import test from "node:test";
import assert from "node:assert/strict";
import { loadBundle } from "./harness.mjs";

const ui = await loadBundle();
const entry = { pinned: false, id: "a".repeat(64), versionId: "1.7.10", platform: "windows-x64", resourceCount: 42, bytes: 12582912, path: "F:/SHTMCL/SHTMCL-data/cache/environments/recipe.json" };

test("cache management errors explain busy and retained environments without exposing diagnostics", () => {
  assert.match(ui.environmentActionError("remove", {code:"launch.install.busy"}), /任务结束/);
  assert.match(ui.environmentActionError("remove", {code:"launch.environment.pinned"}), /取消保留/);
  assert.match(ui.environmentActionError("repair", {code:"launch.environment.unavailable", detailRef:"private-detail"}), /未能开始校验/);
  assert.doesNotMatch(ui.environmentActionError("pin", new Error("private-detail")), /private-detail/);
});

test("cached environments preserve actual identity, resource counts, bytes and path", async () => {
  const requests = [];
  const api = ui.environmentApi(async (request) => {
    requests.push(request);
    return { type: "completed", result: { entries: [entry] } };
  });
  assert.deepEqual(await api.list(), [entry]);
  assert.deepEqual(requests[0].payload, { command: "launch.environments.list" });
  assert.equal(requests[0].operationId, undefined);
  assert.deepEqual(ui.parseCachedEnvironments([]), []);
  assert.deepEqual(ui.parseCachedEnvironments([{ ...entry, bytes: 0, resourceCount: 0 }]), [{ ...entry, bytes: 0, resourceCount: 0 }]);
});

test("bad cached metadata and response envelopes fail instead of becoming an empty list", async () => {
  for (const value of [null, {}, [null], [{ ...entry, id: "../bad" }], [{ ...entry, versionId: " " }],
    [{ ...entry, platform: "linux-x64" }], [{ ...entry, resourceCount: -1 }], [{ ...entry, bytes: 0.5 }],
    [{ ...entry, bytes: Number.MAX_SAFE_INTEGER + 1 }], [{ ...entry, path: "" }], [entry, entry]]) {
    assert.throws(() => ui.parseCachedEnvironments(value));
  }
  for (const response of [null, { type: "accepted", taskId: "t" }, { type: "completed", result: null }, { type: "completed", result: {} }]) {
    await assert.rejects(ui.environmentApi(async () => response).list());
  }
});

test("catalogue preserves healthy entries and explicitly reports skipped recipes", async () => {
  const api = ui.environmentApi(async () => ({ type: "completed", result: { entries: [entry], skippedEntries: 2 } }));
  assert.deepEqual(await api.catalogue(), { entries: [entry], skippedEntries: 2 });
  assert.deepEqual(await api.list(), [entry]);
  const legacy = ui.environmentApi(async () => ({ type: "completed", result: { entries: [entry] } }));
  assert.deepEqual(await legacy.catalogue(), { entries: [entry], skippedEntries: 0 });
  for (const skippedEntries of [null, -1, 0.5, "1", Number.MAX_SAFE_INTEGER + 1]) {
    const malformed = ui.environmentApi(async () => ({ type: "completed", result: { entries: [entry], skippedEntries } }));
    await assert.rejects(malformed.catalogue());
    await assert.rejects(malformed.list());
  }
});

test("download-only requests have unique operation IDs and no instance mutation fields", async () => {
  const requests = [];
  const api = ui.environmentApi(undefined, async (request) => {
    requests.push(request);
    return { type: "accepted", operationId: request.operationId, taskId: "download-task" };
  });
  assert.equal(await api.download("1.7.10"), "download-task");
  await api.download("1.7.10");
  assert.deepEqual(requests[0].payload, { command: "launch.environments.download", versionId: "1.7.10" });
  assert.notEqual(requests[0].requestId, requests[0].operationId);
  assert.notEqual(requests[0].operationId, requests[1].operationId);
  await assert.rejects(api.download(" "));
  assert.equal(requests.length, 2);
});

test("download acceptance rejects malformed or unrelated responses", async () => {
  for (const response of [null, { type: "completed", result: {} }, { type: "accepted", taskId: "" },
    { type: "accepted", operationId: "other", taskId: "task" }]) {
    await assert.rejects(ui.environmentApi(undefined, async () => response).download("1.7.10"));
  }
});

test("browser library has an honest empty state and does not pretend to have local recipes", () => {
  const markup = ui.renderEnvironmentLibrary();
  assert.match(markup, /还没有下载游戏/);
  assert.match(markup, /打开桌面启动器后/);
  assert.match(markup, /使用前校验资源；Java和账户在启动时准备/);
  assert.doesNotMatch(markup, /用它创建实例|缓存位置：|recipe\.json|1\.7\.10/);
});


test("legacy recipes default to unpinned and explicit pin state is validated", () => {
  const { pinned, ...legacy } = entry;
  assert.deepEqual(ui.parseCachedEnvironments([legacy]), [entry]);
  assert.equal(ui.parseCachedEnvironments([{ ...entry, pinned: true }])[0].pinned, true);
  for (const pinned of [null, 1, "false"]) {
    assert.throws(() => ui.parseCachedEnvironments([{ ...entry, pinned }]));
  }
});

test("cache mutations use immutable environment IDs and unique UUID request identities", async () => {
  const requests = [];
  const api = ui.environmentApi(undefined, async (request) => {
    requests.push(request);
    const { command, environmentId, pinned } = request.payload;
    if (command === "launch.environments.repair") return { type: "accepted", taskId: "repair-task", operationId: request.operationId };
    return { type: "completed", result: command === "launch.environments.pin"
      ? { environmentId, pinned } : { environmentId, removed: true } };
  });
  await api.pin(entry.id, true);
  await api.pin(entry.id, false);
  await api.remove(entry.id);
  assert.equal(await api.repair(entry.id), "repair-task");
  assert.deepEqual(requests.map((request) => request.payload), [
    { command: "launch.environments.pin", environmentId: entry.id, pinned: true },
    { command: "launch.environments.pin", environmentId: entry.id, pinned: false },
    { command: "launch.environments.remove", environmentId: entry.id },
    { command: "launch.environments.repair", environmentId: entry.id },
  ]);
  const identities = requests.flatMap((request) => [request.requestId, request.operationId]);
  assert.equal(new Set(identities).size, identities.length);
  for (const identity of identities) assert.match(identity, /^[a-f0-9]{8}-[a-f0-9]{4}-4[a-f0-9]{3}-[89ab][a-f0-9]{3}-[a-f0-9]{12}$/i);
  await assert.rejects(api.pin("../other", true));
  await assert.rejects(api.remove(""));
  await assert.rejects(api.repair("1.7.10"));
  await assert.rejects(api.pin(entry.id, "true"));
  assert.equal(requests.length, 4);
});

test("cache mutation responses must match actual identity, outcome and accepted operation", async () => {
  for (const response of [null, { type: "accepted", taskId: "task" }, { type: "completed", result: {} },
    { type: "completed", result: { environmentId: "b".repeat(64), pinned: true, removed: true } }]) {
    const api = ui.environmentApi(undefined, async () => response);
    await assert.rejects(api.pin(entry.id, true));
    await assert.rejects(api.remove(entry.id));
  }
  await assert.rejects(ui.environmentApi(undefined, async () => ({ type: "completed", result: { environmentId: entry.id, pinned: false } })).pin(entry.id, true));
  await assert.rejects(ui.environmentApi(undefined, async () => ({ type: "completed", result: { environmentId: entry.id, removed: false } })).remove(entry.id));
  for (const response of [null, { type: "completed", result: {} }, { type: "accepted", operationId: "other", taskId: "task" }]) {
    await assert.rejects(ui.environmentApi(undefined, async () => response).repair(entry.id));
  }
  const api = ui.environmentApi(undefined, async (request) => ({ type: "accepted", operationId: request.operationId, taskId: "" }));
  await assert.rejects(api.repair(entry.id));
});
