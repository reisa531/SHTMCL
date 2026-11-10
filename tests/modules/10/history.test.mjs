import test from "node:test";
import assert from "node:assert/strict";
import { loadBundle } from "./harness.mjs";

const ui = await loadBundle();
const entry = {
  operationId: "install-one", taskId: "task-1", name: "校园生存", version: "1.7.10",
  resourceType: "game_environment",
  destination: { kind: "instance", instanceId: "school", instanceName: "校园生存", path: "F:/games/school" },
};
const record = (state, extra = {}) => ({ ...entry, history: { state, phase: "assets", attemptGeneration: 3, ...extra } });
const live = { id: entry.taskId, operationId: entry.operationId, attemptGeneration: 4, state: "running", phase: "assets", cancelRequested: false };

test("instance recovery requires backend inputs and stopped source identity", () => {
  const saved = { ...record("cancelled"), canRetryInstall: true };
  assert.equal(ui.parseDownloadPresentations([saved])[0].canRetryInstall, true);
  assert.equal(ui.canRetryInstanceInstall(saved), true);
  assert.equal(ui.canRetryInstanceInstall(record("cancelled")), false);
  assert.equal(ui.canRetryInstanceInstall({ ...saved, destination: { kind: "cache" } }), false);
  assert.equal(ui.canRetryInstanceInstall(saved, live), false);
  assert.equal(ui.canRetryInstanceInstall(saved, { ...live, state: "failed" }), true);
  assert.equal(ui.canRetryInstanceInstall(saved, { ...live, id: "reused", state: "failed" }), false);
  assert.equal(ui.canRetryInstanceInstall({ ...saved, history: { ...saved.history, activeElsewhere: true } }), false);
});

test("instance retry sends identity only and requires a matching accepted response", async () => {
  let submitted;
  assert.equal(await ui.retryInstallation(entry.operationId, entry.taskId, async (request) => {
    submitted = request;
    return { type: "accepted", operationId: request.operationId, taskId: "new-task" };
  }), "new-task");
  assert.deepEqual(submitted.payload, { command: "launch.installations.retry", sourceOperationId: entry.operationId, sourceTaskId: entry.taskId });
  assert.notEqual(submitted.operationId, entry.operationId);
  await assert.rejects(() => ui.retryInstallation(entry.operationId, entry.taskId, async () => ({type: "accepted", operationId: "wrong", taskId: "new-task"})));
});

test("history uses exact live operation/task identity across retry generations", () => {
  assert.deepEqual(ui.archivedDownloads([record("succeeded")], [live]), []);
  assert.equal(ui.archivedDownloads([record("succeeded")], [{ ...live, attemptGeneration: 1 }]).length, 0);
  assert.equal(ui.archivedDownloads([record("succeeded")], [{ ...live, operationId: "other" }]).length, 1);
  assert.equal(ui.archivedDownloads([record("succeeded")], [{ ...live, id: "child" }]).length, 1);
  assert.equal(ui.archivedDownloads([entry, entry], []).length, 1);
});

test("only durable succeeded is completed; legacy identity cannot imply success", () => {
  assert.equal(ui.archivedDownloads([record("succeeded")], [])[0].group, "completed");
  for (const state of ["failed", "cancelled", "running", "queued", "paused", "waiting_user"]) {
    assert.equal(ui.archivedDownloads([record(state)], [])[0].group, "unfinished", state);
  }
  const legacy = ui.archivedDownloads([{ ...entry, state: "succeeded" }], [])[0];
  assert.equal(legacy.group, "unfinished");
  assert.equal(legacy.status, "状态未记录");
});

test("archived interrupted and other-window activity never masquerade as local running", () => {
  assert.equal(ui.archivedDownloadStatus(record("running", { interrupted: true })), "下载已中断");
  assert.equal(ui.archivedDownloadStatus(record("running", { activeElsewhere: true })), "在另一窗口中进行");
  assert.equal(ui.archivedDownloadStatus(record("running")), "等待恢复");
  assert.equal(ui.archivedDownloadStatus(record("failed", { errorCode: "DOWNLOAD_TIMEOUT", interrupted: true })), "下载失败");
  assert.equal(ui.archivedDownloadStatus(record("succeeded", { activeElsewhere: true, interrupted: true })), "已完成");
});

test("history markup has real destination and collapsed failure detail, without task commands", () => {
  const markup = ui.renderDownloadHistory([record("failed", { errorCode: "DOWNLOAD_TIMEOUT" })], [], "unfinished");
  assert.match(markup, /校园生存.*1\.7\.10/);
  assert.match(markup, /启动包 → 实例 · 校园生存/);
  assert.match(markup, /F:\/games\/school/);
  const details = markup.match(/<details[^>]*>[\s\S]*?<\/details>/)?.[0];
  assert.ok(details);
  assert.match(details, /DOWNLOAD_TIMEOUT/);
  assert.doesNotMatch(details, /<details[^>]*\bopen/);
  assert.doesNotMatch(markup.replace(details, ""), /DOWNLOAD_TIMEOUT/);
  assert.match(markup, /复制路径/);
  assert.doesNotMatch(markup, /取消下载|正在取消|data-attempt-generation|打开实例目录|role="progressbar"/);
  assert.equal(ui.renderDownloadHistory([entry], [], "completed"), "");
  assert.equal(ui.renderDownloadHistory([record("failed")], [live], "unfinished"), "");
});

test("cache retry requires known stopped work and never converts instance installation", () => {
  const cached = { ...record("cancelled"), destination: { kind: "cache" } };
  assert.equal(ui.canRetryCachedDownload(cached), true);
  assert.equal(ui.canRetryCachedDownload(record("cancelled")), false);
  assert.equal(ui.canRetryCachedDownload({ ...cached, history: undefined }), false);
  assert.equal(ui.canRetryCachedDownload({ ...cached, history: { ...cached.history, activeElsewhere: true } }), false);
  assert.equal(ui.canRetryCachedDownload({ ...cached, version: "" }), false);
  for (const state of ["queued", "running", "paused", "waiting_user", "succeeded"]) {
    assert.equal(ui.canRetryCachedDownload(cached, { ...live, state }), false, state);
  }
  assert.equal(ui.canRetryCachedDownload(cached, { ...live, state: "failed" }), true);
  assert.equal(ui.canRetryCachedDownload(cached, { ...live, state: "cancelled", operationId: "other" }), false);
  assert.equal(ui.canRetryCachedDownload({ ...cached, history: { ...cached.history, state: "running", interrupted: true } }), true);
});

test("unfinished cached history offers a fresh request without modifying old task", () => {
  const cached = { ...record("failed"), destination: { kind: "cache" } };
  const markup = ui.renderDownloadHistory([cached], [], "unfinished");
  assert.match(markup, /重新下载/);
  assert.doesNotMatch(markup, /取消下载|data-attempt-generation/);
  assert.doesNotMatch(ui.renderDownloadHistory([{ ...cached, history: { ...cached.history, state: "succeeded" } }], [], "completed"), /重新下载/);
  assert.doesNotMatch(ui.renderDownloadHistory([{ ...cached, history: undefined }], [], "unfinished"), /重新下载/);
});
