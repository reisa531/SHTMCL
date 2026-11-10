// Incremental task stream.
//
// The snapshot is the baseline; live `task.state` / `task.progress` /
// `task.result` events move single rows. Everything here is pure so the
// ordering rules (dedupe by `eventId`, per-operation `sequence`, attempt
// generation) can be asserted without a DOM or a Tauri host.
//
// Rejected events return the *same* state object: the shell can then skip a
// re-render, and a test can prove nothing was applied.

import type { EventEnvelope, ProgressUnit, Task } from "../../contract";
import {
  EVENT_KIND_TASK_PROGRESS,
  EVENT_KIND_TASK_RESULT,
  EVENT_KIND_TASK_STATE,
  type TaskProgressPayload,
  type TaskResultPayload,
  type TaskStatePayload,
} from "../../contract";
import { isNewerThanCursor } from "../../bridge";

/** Event ids kept for duplicate rejection. */
export const EVENT_ID_MEMORY = 128;

export interface TaskStreamGap {
  operationId: string;
  /** Last contiguous sequence applied for that operation. */
  afterSequence: number;
}

export interface TaskStreamState {
  tasks: readonly Task[];
  appliedEventIds: readonly string[];
  /** Highest contiguous sequence per `operationId`. */
  cursors: Readonly<Record<string, number>>;
  /**
   * Set when a live event skips sequences. The caller replays
   * `readEvents(operationId, afterSequence)`; a truncated replay must fall back
   * to a fresh snapshot instead of stitching history.
   */
  gap: TaskStreamGap | null;
  /** Set when an event names a task the shell has never seen. */
  resyncRequested: boolean;
}

export const INITIAL_TASK_STREAM: TaskStreamState = {
  tasks: [],
  appliedEventIds: [],
  cursors: {},
  gap: null,
  resyncRequested: false,
};

export type TaskStreamAction =
  | { type: "snapshot"; tasks: readonly Task[] }
  | { type: "event"; event: EventEnvelope }
  | { type: "gap-cleared"; gap: TaskStreamGap }
  | { type: "resync-handled" };

const TASK_KINDS = new Set<string>([
  EVENT_KIND_TASK_STATE,
  EVENT_KIND_TASK_PROGRESS,
  EVENT_KIND_TASK_RESULT,
]);

function isRecord(value: unknown): value is Record<string, unknown> {
  return typeof value === "object" && value !== null;
}

function taskIdOf(payload: unknown): string | null {
  if (!isRecord(payload)) {
    return null;
  }
  const taskId = payload["taskId"];
  return typeof taskId === "string" ? taskId : null;
}

function mergeTask(task: Task, event: EventEnvelope): Task | null {
  switch (event.kind) {
    case EVENT_KIND_TASK_STATE: {
      if (!isRecord(event.payload)) {
        return null;
      }
      const payload = event.payload as unknown as TaskStatePayload;
      return {
        ...task,
        attemptGeneration: event.attemptGeneration,
        state: payload.state,
        phase: payload.phase,
        cancelRequested: payload.cancelRequested,
      };
    }
    case EVENT_KIND_TASK_PROGRESS: {
      if (!isRecord(event.payload)) {
        return null;
      }
      const payload = event.payload as unknown as TaskProgressPayload;
      return {
        ...task,
        attemptGeneration: event.attemptGeneration,
        progress: {
          current: payload.current,
          total: payload.total,
          unit: payload.unit as ProgressUnit,
        },
      };
    }
    case EVENT_KIND_TASK_RESULT: {
      if (!isRecord(event.payload)) {
        return null;
      }
      const payload = event.payload as unknown as TaskResultPayload;
      const next: Task = {
        ...task,
        attemptGeneration: event.attemptGeneration,
        state: payload.state,
      };
      if (payload.error !== undefined) {
        next.error = payload.error;
      }
      if (payload.result !== undefined) {
        next.result = payload.result;
      }
      return next;
    }
    default:
      return null;
  }
}

function applyEvent(
  state: TaskStreamState,
  event: EventEnvelope,
): TaskStreamState {
  if (!TASK_KINDS.has(event.kind)) {
    return state;
  }

  const taskId = taskIdOf(event.payload);
  if (taskId === null) {
    return state;
  }

  if (state.appliedEventIds.includes(event.eventId)) {
    return state;
  }

  const lastSequence = state.cursors[event.operationId];
  const cursor =
    lastSequence === undefined
      ? null
      : { operationId: event.operationId, sequence: lastSequence };

  if (!isNewerThanCursor(event, cursor)) {
    return state;
  }

  if (lastSequence !== undefined && event.sequence > lastSequence + 1) {
    // Do not apply the later event first: replay the hole, then continue.
    return {
      ...state,
      gap: { operationId: event.operationId, afterSequence: lastSequence },
    };
  }

  const index = state.tasks.findIndex((task) => task.id === taskId);
  if (index < 0) {
    return { ...state, resyncRequested: true };
  }

  const current = state.tasks[index];
  if (event.attemptGeneration < current.attemptGeneration) {
    return state;
  }

  const updated = mergeTask(current, event);
  if (!updated) {
    return state;
  }

  const tasks = state.tasks.slice();
  tasks[index] = updated;

  return {
    tasks,
    appliedEventIds: [...state.appliedEventIds, event.eventId].slice(
      -EVENT_ID_MEMORY,
    ),
    cursors: { ...state.cursors, [event.operationId]: event.sequence },
    gap: null,
    resyncRequested: false,
  };
}

export function reduceTaskStream(
  state: TaskStreamState,
  action: TaskStreamAction,
): TaskStreamState {
  switch (action.type) {
    case "snapshot":
      return {
        tasks: action.tasks,
        appliedEventIds: [],
        cursors: state.cursors,
        gap: null,
        resyncRequested: false,
      };
    case "event":
      return applyEvent(state, action.event);
    case "gap-cleared":
      if (
        !state.gap ||
        state.gap.operationId !== action.gap.operationId ||
        state.gap.afterSequence !== action.gap.afterSequence
      ) {
        return state;
      }
      return { ...state, gap: null };
    case "resync-handled":
      return state.resyncRequested
        ? { ...state, resyncRequested: false }
        : state;
  }
}
