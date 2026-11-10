import test from "node:test";
import assert from "node:assert/strict";
import { loadBundle } from "./harness.mjs";

const ui = await loadBundle();
const entry = {
  operationId: "install-one", taskId: "task-1", name: "校园生存", version: "1.7.10",
  resourceType: "game_environment",
  destination: { kind: "instance", instanceId: "school", instanceName: "校园生存", path: "F:\\games\\school" },
};
const task = {
  id: entry.taskId, operationId: entry.operationId, attemptGeneration: 1,
  state: "running", phase: "assets", cancelRequested: false,
  progress: { current: 40, total: 100, unit: "items" },
};

test("operations use identified parents, deduplicate children/retries and exclude non-downloads", () => {
  const child = { ...task, id: "asset-1", state: "succeeded", attemptGeneration: 9 };
  const unrelated = { ...task, id: "refresh-1", operationId: "refresh" };
  const older = { ...task, state: "succeeded", attemptGeneration: 0 };
  const operations = ui.downloadOperations([older, task, child, unrelated], [entry, entry]);
  assert.equal(operations.length, 1);
  assert.equal(operations[0].task.state, "running");
  assert.equal(operations[0].group, "active");
  assert.equal(operations[0].children.length, 1);
  assert.deepEqual(ui.downloadOperations([child], [entry]), [], "a successful child never stands in for a missing parent");
  assert.deepEqual(ui.downloadOperations([{ ...task, operationId: "different" }], [entry]), []);
});

test("three download groups and navigation badge have a consistent activity definition", () => {
  for (const state of ["queued", "running"]) assert.equal(ui.downloadGroup({ ...task, state }), "active");
  for (const state of ["paused", "waiting_user", "failed", "cancelled"]) assert.equal(ui.downloadGroup({ ...task, state }), "unfinished");
  assert.equal(ui.downloadGroup({ ...task, state: "succeeded" }), "completed");
  assert.doesNotMatch(ui.renderDownloadBadge(0), /class="nav__badge"/);
  assert.match(ui.renderDownloadBadge(2), /任务，2个进行中的下载/);
  assert.match(ui.renderDownloadBadge(100), /99\+/);
});

test("download page hides unrelated background tasks and exposes three tabs", () => {
  const markup = ui.renderShell("tasks", { tasks: [task, { ...task, id: "secret-refresh", operationId: "refresh" }], presentations: [entry] });
  assert.match(markup, /校园生存/);
  assert.match(markup, /未完成/);
  assert.doesNotMatch(markup, /secret-refresh|后台任务/);
  const empty = ui.renderShell("tasks", { tasks: [task] });
  assert.doesNotMatch(empty, /task-task-1/);
});

test("child progress does not invalidate another operation's row memo", () => {
  const props = ui.taskRowPropsFor(task, {}, new Set(), () => {}, () => {});
  const child = { ...task, id: "child" };
  assert.equal(ui.taskRowPropsAreEqual({ ...props, relatedTasks: [child] }, { ...props, relatedTasks: [{ ...child, progress: { current: 9, total: 10, unit: "items" } }] }), true);
  assert.equal(ui.taskRowPropsAreEqual({ ...props, relatedTasks: [child] }, { ...props, relatedTasks: [{ ...child, state: "failed" }] }), false);
});

test("download decoration respects reduced motion and cleans up on resize", () => {
  const previousWindow = globalThis.window, previousDocument = globalThis.document;
  const listeners = new Map(), timers = new Map(), target = { dataset: {}, getBoundingClientRect: () => ({ left: 20, top: 200, width: 160, height: 40 }) };
  let reduced = false, removed = false, cancelled = false, frames;
  const ball = { setAttribute() {}, remove() { removed = true; }, animate(value) { frames = value; return { finished: new Promise(() => {}), cancel() { cancelled = true; } }; } };
  try {
    globalThis.window = { innerWidth: 1200, innerHeight: 800, matchMedia: () => ({ matches: reduced }),
      setTimeout: (callback) => { timers.set(1, callback); return 1; }, clearTimeout: (id) => timers.delete(id),
      addEventListener: (name, callback) => listeners.set(name, callback), removeEventListener: (name) => listeners.delete(name) };
    globalThis.document = { hidden: false, documentElement: { dataset: {} }, querySelector: () => target,
      createElement: () => ball, body: { append() {} }, addEventListener: (name, callback) => listeners.set(name, callback), removeEventListener: (name) => listeners.delete(name) };
    ui.downloadFlight({ x: 900, y: 500 });
    assert.equal(frames.length, 3, "flight has a raised midpoint and a destination");
    assert.equal(target.dataset.downloadFeedback, "true");
    listeners.get("resize")();
    assert.equal(removed && cancelled, true);
    assert.equal(listeners.size, 0);
    assert.equal(timers.size, 0);
    assert.equal(target.dataset.downloadFeedback, undefined);
    reduced = true; frames = undefined;
    const cleanup = ui.downloadFlight({ x: 900, y: 500 });
    assert.equal(frames, undefined, "reduced motion uses only a static highlight");
    cleanup();
    globalThis.document.hidden = true;
    ui.downloadFlight({ x: 900, y: 500 });
    assert.equal(timers.size, 0, "hidden windows do not start decoration");
  } finally {
    if (previousWindow === undefined) delete globalThis.window; else globalThis.window = previousWindow;
    if (previousDocument === undefined) delete globalThis.document; else globalThis.document = previousDocument;
  }
});

test("download projection parser keeps real identity and paths, skips unsupported or malformed records", () => {
  assert.deepEqual(ui.parseDownloadPresentations([entry]), [entry]);
  assert.deepEqual(ui.parseDownloadPresentations([
    null, { ...entry, resourceType: "constructor" }, { ...entry, destination: { kind: "any" } },
    { ...entry, name: "" }, { ...entry, version: 123 }, { ...entry, state: "ready" },
    entry, entry,
  ]), [entry]);
  assert.deepEqual(ui.parseDownloadPresentations({ entries: [entry] }), []);
});

test("task IDs from another operation never inherit a resource or destination", () => {
  assert.equal(ui.presentationForTask(task, [entry]), entry);
  assert.equal(ui.presentationForTask({ ...task, operationId: "different-run" }, [entry]), undefined);
  const markup = ui.renderPresentedTasks([{ ...task, operationId: "different-run" }], [entry]);
  assert.doesNotMatch(markup, /校园生存|F:\\games/);
});

test("resource name, destination and percent are primary; technical data is inside closed details", () => {
  const markup = ui.renderPresentedTasks([task], [entry]);
  const details = markup.match(/<details[^>]*>[\s\S]*?<\/details>/)?.[0];
  assert.ok(details);
  assert.doesNotMatch(details, /<details[^>]*\bopen/);
  assert.match(details, /task-1/);
  assert.match(details, /assets/);
  assert.match(details, /40 \/ 100 items/);
  const main = markup.replace(details, "");
  assert.match(main, /校园生存.*1\.7\.10/);
  assert.match(main, /启动包 → 实例 · 校园生存/);
  assert.match(main, /F:\\games\\school/);
  assert.match(main, /40%/);
  assert.doesNotMatch(main, />task-1<|>assets<|40 \/ 100 items/);
  assert.match(main, /取消任务/);
});

test("cached Mods explicitly remain unadded and failed operations do not claim success", () => {
  const mod = { ...entry, resourceType: "mod", destination: { kind: "cache", path: "F:\\cache" } };
  assert.match(ui.renderPresentedTasks([{ ...task, state: "succeeded" }], [mod]), /仅下载 · 未添加到实例/);
  const failed = ui.renderPresentedTasks([{ ...task, state: "failed", error: { code: "download.hash", retryable: true } }], [entry]);
  assert.match(failed, /任务未完成/);
  assert.doesNotMatch(failed, /已添加|已安装/);
});

test("unknown totals stay indeterminate and unknown phases have a short neutral label", () => {
  const markup = ui.renderPresentedTasks([{ ...task, phase: "private-implementation-phase", progress: { current: 2, total: null, unit: "steps" } }], [entry]);
  assert.match(markup, /data-indeterminate="true"/);
  assert.doesNotMatch(markup, /aria-valuenow=/);
  assert.match(markup, /处理中/);
  assert.equal(ui.downloadPhaseLabel("private"), "处理中");
});

