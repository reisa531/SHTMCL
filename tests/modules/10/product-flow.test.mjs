import test from "node:test";
import assert from "node:assert/strict";
import { loadBundle } from "./harness.mjs";
const ui = await loadBundle();

test("instance list has a creation entry and no embedded version downloader", () => {
  const markup = ui.renderInstancesWithCreate();
  assert.match(markup, /安装游戏/);
  assert.doesNotMatch(markup, /搜索版本|game-version|version-picker/);
  const create = ui.renderCreationPage();
  assert.match(create, /创建实例/);
  assert.match(create, /返回实例/);
  assert.match(create, /搜索版本/);
  for (const family of ["release", "snapshot", "old_alpha", "old_beta"]) assert.match(create, new RegExp(`value="${family}"`));
});

test("download row exposes real resource and destination, with technical data collapsed", () => {
  const task = { ...ui.TASKS.running, operationId: "create-campus", phase: "assets" };
  const entry = { operationId: task.operationId, taskId: task.id, name: "校园生存", version: "1.7.10", resourceType: "game_environment", destination: { kind: "instance", instanceId: "campus", instanceName: "校园生存", path: "F:/games/campus" } };
  const markup = ui.renderPresentedTasks([task], [entry]);
  assert.match(markup, /校园生存/);
  assert.match(markup, /F:\/games\/campus/);
  assert.match(markup, /<details/);
  assert.doesNotMatch(markup, /<details[^>]*\bopen\b/);
  assert.match(markup, /取消任务/);
  assert.equal(ui.presentationForTask({ ...task, operationId: "other-operation" }, [entry]), undefined);
});
