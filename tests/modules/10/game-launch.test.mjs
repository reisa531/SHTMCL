import test from "node:test";
import assert from "node:assert/strict";
import { loadBundle } from "./harness.mjs";

const { gameLaunchApi, launchError, renderLocalIdentityForm } = await loadBundle();
const steve = { kind: "offline", username: "Steve", uuid: "5627dd98e6be3c21b8a8e92344183641" };

test("identity lookup represents an absent choice without inventing Player", async () => {
  const requests = [];
  const api = gameLaunchApi(async (request) => {
    requests.push(request);
    return { type: "completed", result: { identity: null } };
  });
  assert.equal(await api.currentIdentity(), null);
  assert.deepEqual(requests[0].payload, { command: "launch.identity.current" });
  assert.equal(requests[0].operationId, undefined);
});

test("explicit selection submits only local identity and validates returned facts", async () => {
  const requests = [];
  const api = gameLaunchApi(undefined, async (request) => {
    requests.push(request);
    return { type: "completed", result: { identity: steve } };
  });
  assert.deepEqual(await api.selectOffline("Steve"), steve);
  assert.deepEqual(requests[0].payload, { command: "launch.identity.select", kind: "offline", username: "Steve" });
  assert.ok(requests[0].operationId);
  assert.notEqual(requests[0].requestId, requests[0].operationId);
});

test("invalid identity responses never produce a selected account", async () => {
  const invalid = [
    { type: "accepted", operationId: "op", taskId: "task" },
    { type: "completed", result: null },
    { type: "completed", result: {} },
    { type: "completed", result: { identity: { ...steve, kind: "mua" } } },
    { type: "completed", result: { identity: { ...steve, uuid: "not-a-uuid" } } },
    { type: "completed", result: { identity: { ...steve, username: "name with space" } } },
  ];
  for (const response of invalid) {
    const api = gameLaunchApi(async () => response, async () => response);
    await assert.rejects(() => api.currentIdentity());
    await assert.rejects(() => api.selectOffline("Steve"));
  }
  await assert.rejects(() => gameLaunchApi(undefined, async () => ({ type: "completed", result: { identity: null } })).selectOffline("Steve"));
});

test("launch starts one parent operation and returns both accepted identifiers", async () => {
  const requests = [];
  const api = gameLaunchApi(undefined, async (request) => {
    requests.push(request);
    return { type: "accepted", operationId: request.operationId, taskId: "parent-task" };
  });
  const accepted = await api.launch("campus");
  assert.equal(accepted.taskId, "parent-task");
  assert.equal(accepted.operationId, requests[0].operationId);
  assert.deepEqual(requests[0].payload, { command: "launch.game.start", instanceId: "campus" });
  assert.equal(requests.length, 1, "UI must not independently prepare Java or spawn a second operation");
});

test("launch refuses completed or malformed accepted responses", async () => {
  for (const response of [
    { type: "completed", result: { started: true } },
    { type: "accepted", operationId: "op" },
    { type: "accepted", operationId: "", taskId: "task" },
    { type: "accepted", operationId: "op", taskId: "" },
  ]) await assert.rejects(() => gameLaunchApi(undefined, async () => response).launch("campus"));
});

test("local identity form uses native validation and disables unavailable or pending choices", () => {
  const valid = renderLocalIdentityForm({ username: "Steve", connected: true, pending: false });
  assert.match(valid, /<label[^>]*>本地玩家名称/);
  assert.match(valid, /required=""/);
  assert.match(valid, /maxLength="16"|maxlength="16"/);
  assert.match(valid, /pattern="\[A-Za-z0-9_\]\{1,16\}"/);
  assert.doesNotMatch(valid, /disabled=""/);
  assert.match(valid, /type="submit"/);

  for (const options of [
    { username: "", connected: true, pending: false },
    { username: "two players", connected: true, pending: false },
    { username: "Steve", connected: false, pending: false },
  ]) assert.match(renderLocalIdentityForm(options), /<button[^>]*disabled=""/);

  const pending = renderLocalIdentityForm({ username: "Steve", connected: true, pending: true });
  assert.match(pending, /<input[^>]*disabled=""/);
  assert.match(pending, /<button[^>]*disabled=""[^>]*aria-busy="true"/);
  assert.match(pending, /正在选择/);
});

test("identity errors stay distinct from provider authentication and raw failures", () => {
  assert.match(launchError({ code: "launch.identity.required" }), /选择本地玩家/);
  assert.match(launchError({ code: "launch.identity.changed" }), /重新启动/);
  assert.match(launchError({ code: "launch.identity.online_required" }), /在线账户/);
  assert.doesNotMatch(launchError(new Error("private diagnostics")), /private diagnostics/);
});
