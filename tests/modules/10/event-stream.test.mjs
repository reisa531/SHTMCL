// Module 10 · incremental task stream assertions.
//
// The reducer is pure, so ordering rules are asserted directly: duplicate
// `eventId`, per-operation `sequence`, attempt-generation fences, sequence
// holes, unknown tasks and unknown kinds.

import test from "node:test";
import assert from "node:assert/strict";

import { loadBundle } from "./harness.mjs";

const ui = await loadBundle();

const RUNNING = ui.TASKS.running; // task-1, operation op-1, attempt 2

function stateEvent(overrides = {}) {
  return ui.makeEvent({
    eventId: "event-1",
    sequence: 1,
    attemptGeneration: 2,
    kind: "task.state",
    payload: {
      taskId: "task-1",
      state: "waiting_user",
      phase: "await_choice",
      cancelRequested: false,
    },
    ...overrides,
  });
}

test("a snapshot sets the baseline and later events move one row", () => {
  const base = ui.streamWith([RUNNING]);
  assert.equal(base.tasks.length, 1);
  assert.equal(base.tasks[0].phase, "download");

  const next = ui.reduceTaskStream(base, {
    type: "event",
    event: stateEvent(),
  });

  assert.match(next.tasks[0].phase, /await_choice/);
  assert.equal(next.tasks[0].state, "waiting_user");
  assert.equal(next.cursors["op-1"], 1);
  assert.deepEqual(next.appliedEventIds, ["event-1"]);
  // Snapshot identity is preserved for untouched tasks.
  assert.equal(base.tasks[0].phase, "download", "reducer is immutable");
});

test("the same eventId is applied once", () => {
  const base = ui.streamWith([RUNNING]);
  const first = ui.reduceTaskStream(base, {
    type: "event",
    event: stateEvent(),
  });
  const duplicate = ui.reduceTaskStream(first, {
    type: "event",
    event: stateEvent(),
  });

  assert.equal(duplicate, first, "re-delivered event returns the same state");
  assert.equal(duplicate.appliedEventIds.length, 1);
});

test("a duplicate event cannot regress a row after a later event", () => {
  const base = ui.streamWith([RUNNING]);
  const first = ui.reduceTaskStream(base, {
    type: "event",
    event: stateEvent({ eventId: "event-1", sequence: 1 }),
  });
  const second = ui.reduceTaskStream(first, {
    type: "event",
    event: ui.makeEvent({
      eventId: "event-2",
      sequence: 2,
      attemptGeneration: 2,
      kind: "task.progress",
      payload: { taskId: "task-1", current: 900, total: 1024, unit: "bytes" },
    }),
  });
  const replayOfFirst = ui.reduceTaskStream(second, {
    type: "event",
    event: stateEvent({ eventId: "event-1", sequence: 1 }),
  });

  assert.equal(replayOfFirst, second, "older sequence is ignored");
  assert.equal(replayOfFirst.tasks[0].progress.current, 900);
});

test("an older attemptGeneration does not overwrite a newer row", () => {
  const base = ui.streamWith([RUNNING]); // attemptGeneration 2
  const stale = ui.reduceTaskStream(base, {
    type: "event",
    event: stateEvent({ attemptGeneration: 1, sequence: 1 }),
  });

  assert.equal(stale, base, "stale generation is dropped without a new object");
  assert.equal(stale.tasks[0].state, RUNNING.state);
});

test("a newer attemptGeneration takes over the row", () => {
  const base = ui.streamWith([RUNNING]);
  const retried = ui.reduceTaskStream(base, {
    type: "event",
    event: stateEvent({
      attemptGeneration: 3,
      sequence: 1,
      payload: {
        taskId: "task-1",
        state: "queued",
        phase: "queue",
        cancelRequested: false,
      },
    }),
  });

  assert.equal(retried.tasks[0].attemptGeneration, 3);
  assert.equal(retried.tasks[0].state, "queued");
  assert.equal(retried.tasks[0].phase, "queue");
});

test("progress and result events merge into the existing row", () => {
  const base = ui.streamWith([RUNNING]);

  const progressed = ui.reduceTaskStream(base, {
    type: "event",
    event: ui.makeEvent({
      eventId: "event-progress",
      sequence: 1,
      attemptGeneration: 2,
      kind: "task.progress",
      payload: { taskId: "task-1", current: 1024000, total: 1024000, unit: "bytes" },
    }),
  });
  assert.equal(progressed.tasks[0].progress.current, 1024000);
  assert.equal(progressed.tasks[0].state, "running", "state untouched");

  const finished = ui.reduceTaskStream(progressed, {
    type: "event",
    event: ui.makeEvent({
      eventId: "event-result",
      sequence: 2,
      attemptGeneration: 2,
      kind: "task.result",
      payload: {
        taskId: "task-1",
        state: "failed",
        error: {
          code: "download.source_unavailable",
          category: "network",
          retryable: true,
        },
      },
    }),
  });

  assert.equal(finished.tasks[0].state, "failed");
  assert.equal(finished.tasks[0].error.code, "download.source_unavailable");
  assert.equal(finished.tasks[0].progress.current, 1024000, "progress kept");
});

test("a sequence hole is repaired by the caller, not applied out of order", () => {
  const base = ui.streamWith([RUNNING]);
  const first = ui.reduceTaskStream(base, {
    type: "event",
    event: stateEvent({ eventId: "event-1", sequence: 1 }),
  });
  const jump = ui.reduceTaskStream(first, {
    type: "event",
    event: stateEvent({
      eventId: "event-4",
      sequence: 4,
      payload: {
        taskId: "task-1",
        state: "succeeded",
        phase: "commit",
        cancelRequested: false,
      },
    }),
  });

  assert.deepEqual(jump.gap, { operationId: "op-1", afterSequence: 1 });
  assert.equal(jump.tasks[0].state, "waiting_user", "later event not applied");
  assert.equal(jump.cursors["op-1"], 1, "cursor stays contiguous");

  const cleared = ui.reduceTaskStream(jump, {
    type: "gap-cleared",
    gap: { operationId: "op-1", afterSequence: 1 },
  });
  assert.equal(cleared.gap, null);

  const staleClear = ui.reduceTaskStream(jump, {
    type: "gap-cleared",
    gap: { operationId: "op-9", afterSequence: 1 },
  });
  assert.equal(staleClear, jump, "a different gap does not clear this one");
});

test("an event for an unknown task requests a resync", () => {
  const base = ui.streamWith([RUNNING]);
  const unknown = ui.reduceTaskStream(base, {
    type: "event",
    event: stateEvent({
      eventId: "event-x",
      sequence: 1,
      payload: {
        taskId: "task-unknown",
        state: "running",
        phase: "download",
        cancelRequested: false,
      },
    }),
  });

  assert.equal(unknown.resyncRequested, true);
  assert.equal(unknown.tasks.length, 1, "no row is invented");
  assert.equal(unknown.tasks[0].id, "task-1");

  const handled = ui.reduceTaskStream(unknown, { type: "resync-handled" });
  assert.equal(handled.resyncRequested, false);
  assert.equal(
    ui.reduceTaskStream(handled, { type: "resync-handled" }),
    handled,
    "clearing twice is a no-op",
  );
});

test("unknown event kinds and payload shapes are ignored", () => {
  const base = ui.streamWith([RUNNING]);

  const otherKind = ui.reduceTaskStream(base, {
    type: "event",
    event: ui.makeEvent({
      eventId: "event-room",
      sequence: 1,
      attemptGeneration: 2,
      kind: "room.membership.changed",
      payload: { taskId: "task-1" },
    }),
  });
  assert.equal(otherKind, base);
  assert.equal(otherKind.cursors["op-1"], undefined, "cursor not advanced");

  const noTaskId = ui.reduceTaskStream(base, {
    type: "event",
    event: ui.makeEvent({
      eventId: "event-shape",
      sequence: 1,
      attemptGeneration: 2,
      kind: "task.state",
      payload: { state: "running" },
    }),
  });
  assert.equal(noTaskId, base);
});

test("a snapshot resets dedupe memory but keeps the cursor", () => {
  const base = ui.streamWith([RUNNING]);
  const applied = ui.reduceTaskStream(base, {
    type: "event",
    event: stateEvent(),
  });
  const resynced = ui.reduceTaskStream(applied, {
    type: "snapshot",
    tasks: [RUNNING],
  });

  assert.equal(resynced.appliedEventIds.length, 0);
  assert.equal(resynced.cursors["op-1"], 1);
  assert.equal(resynced.gap, null);
  assert.equal(resynced.resyncRequested, false);
});

test("dedupe memory is bounded", () => {
  assert.ok(ui.EVENT_ID_MEMORY > 0);
  let state = ui.streamWith([RUNNING]);
  for (let index = 0; index < ui.EVENT_ID_MEMORY + 10; index += 1) {
    state = ui.reduceTaskStream(state, {
      type: "event",
      event: ui.makeEvent({
        eventId: `event-${index}`,
        sequence: index + 1,
        attemptGeneration: 2,
        kind: "task.progress",
        payload: { taskId: "task-1", current: index, total: 100, unit: "items" },
      }),
    });
  }
  assert.equal(state.appliedEventIds.length, ui.EVENT_ID_MEMORY);
  assert.equal(state.cursors["op-1"], ui.EVENT_ID_MEMORY + 10);
});

test("the task list renders a stream-updated row", () => {
  const base = ui.streamWith([RUNNING]);
  const streamed = ui.reduceTaskStream(base, {
    type: "event",
    event: ui.makeEvent({
      eventId: "event-1",
      sequence: 1,
      attemptGeneration: 2,
      kind: "task.progress",
      payload: { taskId: "task-1", current: 256000, total: 1024000, unit: "bytes" },
    }),
  });
  const markup = ui.renderTaskList(streamed.tasks);
  assert.match(markup, /aria-valuenow="25"/);
  assert.match(markup, /256000 \/ 1024000 B/);
});
