import test from "node:test";
import assert from "node:assert/strict";
import { loadBundle } from "./harness.mjs";

const { instanceCopyApi, instanceCopyError, encodeInstanceReference, parseInstanceReference } = await loadBundle();

test("instance references are bounded local identities without paths", () => {
  assert.equal(parseInstanceReference(encodeInstanceReference("game-123")), "game-123");
  for (const value of ["{}", "[]", "null", "a".repeat(1025),
    JSON.stringify({ format:"shtmcl-instance-reference", schemaVersion:2, sourceInstanceId:"game-123" }),
    JSON.stringify({ format:"shtmcl-instance-reference", schemaVersion:1, sourceInstanceId:"../other" }),
    JSON.stringify({ format:"shtmcl-instance-reference", schemaVersion:1, sourceInstanceId:"game-123", path:"F:/data" })]) assert.throws(() => parseInstanceReference(value));
});

test("copy submits explicit save choice and correlates accepted operation", async () => {
  const calls = [];
  const api = instanceCopyApi(async (request) => {
    calls.push(request);
    return { type:"accepted", operationId:request.operationId, taskId:"copy-task" };
  });
  const result = await api.copy("source", "target", " 副本 ", false);
  assert.deepEqual(calls[0].payload, { command:"instances.copy", sourceInstanceId:"source", newInstanceId:"target", name:"副本", includeSaves:false });
  assert.equal(result.newInstanceId,"target");
  assert.equal(result.taskId,"copy-task");
  assert.equal(result.operationId,calls[0].operationId);
  assert.notEqual(calls[0].requestId,calls[0].operationId);
  await api.copy("source", "target-2", "副本", true);
  assert.equal(calls[1].payload.includeSaves,true);
});

test("copy rejects malformed requests before dispatch and unrelated acknowledgements", async () => {
  let called = false;
  const api = instanceCopyApi(async () => { called = true; throw Error("should not dispatch"); });
  for (const args of [["source","SOURCE","name",false], ["source","../target","name",false], ["source","CON","name",false], ["source","target"," ",false], ["source","target","x".repeat(129),false], ["source","target","name",undefined]]) await assert.rejects(api.copy(...args));
  assert.equal(called,false);
  for (const response of [null, {type:"completed",result:{}}, {type:"accepted",taskId:"task",operationId:"wrong"}]) {
    await assert.rejects(instanceCopyApi(async () => response).copy("source","target","name",false));
  }
});

test("copy errors give useful actions without leaking diagnostics", () => {
  assert.match(instanceCopyError({code:"instances.instance.locked"}),/结束游戏/);
  assert.match(instanceCopyError({code:"instances.instance.not_found"}),/刷新/);
  assert.doesNotMatch(instanceCopyError({detailRef:"private-path"}),/private-path/);
});
