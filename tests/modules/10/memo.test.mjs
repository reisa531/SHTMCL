// Module 10 · memo boundary / re-render counts (evaluator defect D1).
//
// "One event re-rendered every row" was not something the previous tests could
// see: they asserted element counts and `$$typeof === react.memo`, both true
// while the boundary was broken. These assertions count rows that would render.

import test from "node:test";
import assert from "node:assert/strict";

import { loadBundle } from "./harness.mjs";
import { simulateOneEvent } from "./memo-sim.mjs";

const ui = await loadBundle();

test("the row comparator is the one React uses", () => {
  assert.equal(ui.taskRowComponent.$$typeof, ui.memoType, "TaskRow is memoised");
  assert.equal(
    ui.taskRowComponent.compare,
    ui.taskRowPropsAreEqual,
    "React.memo uses the exported comparator, so the simulation below is faithful",
  );
});

test("one event re-renders one row out of 4", () => {
  const result = simulateOneEvent(ui, 4, 2);
  assert.equal(result.before, 4, "default shallow compare renders every row");
  assert.equal(result.after, 1, "explicit comparator renders one row");
  assert.deepEqual(result.afterIds, [result.targetId]);
});

test("one event re-renders one row out of 40", () => {
  const result = simulateOneEvent(ui, 40, 17);
  assert.equal(result.before, 40, "before the fix: all 40 rows re-render");
  assert.equal(result.after, 1, "after the fix: exactly the touched row");
  assert.deepEqual(result.afterIds, [result.targetId]);
});

test("one event re-renders one row out of 200", () => {
  const result = simulateOneEvent(ui, 200, 199);
  assert.equal(result.before, 200);
  assert.equal(result.after, 1);
  assert.deepEqual(result.afterIds, ["task-200"]);
});

test("rows are compared by content, not object identity", () => {
  const task = {
    id: "task-a",
    operationId: "op-a",
    attemptGeneration: 1,
    state: "running",
    phase: "download",
    progress: { current: 1, total: 10, unit: "bytes" },
    cancelRequested: false,
  };
  const clone = JSON.parse(JSON.stringify(task));
  assert.notEqual(task, clone, "different object");

  const prev = ui.taskRowPropsFor(task, {}, new Set(), () => {}, () => {});
  const next = ui.taskRowPropsFor(clone, {}, new Set(), () => {}, () => {});
  assert.equal(ui.taskRowPropsAreEqual(prev, next), true, "equal content, new object");

  const moved = { ...clone, progress: { ...clone.progress, current: 2 } };
  assert.equal(
    ui.taskRowPropsAreEqual(prev, ui.taskRowPropsFor(moved, {}, new Set(), () => {}, () => {})),
    false,
    "progress change forces a render",
  );

  for (const change of [
    { state: "succeeded" },
    { phase: "verify" },
    { attemptGeneration: 2 },
    { cancelRequested: true },
    { error: { code: "download.source_unavailable", category: "network", retryable: true } },
  ]) {
    const changed = { ...clone, ...change };
    assert.equal(
      ui.taskRowPropsAreEqual(prev, ui.taskRowPropsFor(changed, {}, new Set(), () => {}, () => {})),
      false,
      `change ${JSON.stringify(change)} must force a render`,
    );
  }

  // Render-only inputs are compared too.
  const otherFeedback = ui.taskRowPropsFor(clone, { "task-a": { status: "requested" } }, new Set(), () => {}, () => {});
  assert.equal(ui.taskRowPropsAreEqual(prev, otherFeedback), false, "feedback change renders");
  const pending = ui.taskRowPropsFor(clone, {}, new Set(["task-a"]), () => {}, () => {});
  assert.equal(ui.taskRowPropsAreEqual(prev, pending), false, "pending change renders");
});

test("an unchanged stream leaves every row alone", () => {
  const tasks = [];
  for (let index = 0; index < 40; index += 1) {
    tasks.push({
      id: `task-${index}`,
      operationId: `op-${index}`,
      attemptGeneration: 1,
      state: "running",
      phase: "download",
      progress: { current: index, total: 100, unit: "items" },
      cancelRequested: false,
    });
  }
  const stream = ui.streamWith(tasks);
  const ignored = ui.reduceTaskStream(stream, {
    type: "event",
    event: ui.makeEvent({
      eventId: "duplicate",
      sequence: 1,
      attemptGeneration: 1,
      kind: "task.progress",
      payload: { taskId: "task-0", current: 5, total: 100, unit: "items" },
    }),
  });
  const again = ui.reduceTaskStream(ignored, {
    type: "event",
    event: ui.makeEvent({
      eventId: "duplicate",
      sequence: 1,
      attemptGeneration: 1,
      kind: "task.progress",
      payload: { taskId: "task-0", current: 5, total: 100, unit: "items" },
    }),
  });
  assert.equal(again, ignored, "duplicate event returns the same state object");
  assert.equal(again.tasks[0], ignored.tasks[0], "row objects are untouched");
});
