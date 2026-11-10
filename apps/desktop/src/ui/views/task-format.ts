// Task presentation helpers. Pure: no React, no bridge calls.

import type { CancelDecision, Task, TaskState } from "../../contract";
import { uiLabels } from "../labels";

const ACTIVE_STATES: ReadonlySet<TaskState> = new Set([
  "queued",
  "running",
  "paused",
  "waiting_user",
]);

export function isActiveState(state: TaskState): boolean {
  return ACTIVE_STATES.has(state);
}

export function isTerminalState(state: TaskState): boolean {
  return state === "succeeded" || state === "failed" || state === "cancelled";
}

const INSTALL_PHASES: Readonly<Record<string, string>> = {
  install: "安装游戏", metadata: "读取游戏信息", download_files: "下载游戏与依赖",
  assets: "下载游戏资源", natives: "安装本地运行库", commit: "完成安装", catalog: "更新版本列表",
};
export function taskPhaseLabel(phase: string): string {
  return INSTALL_PHASES[phase] ?? phase;
}

/** Cancelling is offered only while the task can still reach a safe stop. */
export function canCancel(task: Task): boolean {
  return !isTerminalState(task.state) && !task.cancelRequested;
}

/** A task whose progress total the core did not report as a number. */
export function hasUnknownTotal(task: Task): boolean {
  return task.progress !== undefined && task.progress.total === null;
}

export interface TaskSummary {
  itemCount: number;
  activeCount: number;
  unknownTotalCount: number;
}

export function summarizeTasks(tasks: readonly Task[]): TaskSummary {
  let activeCount = 0;
  let unknownTotalCount = 0;
  for (const task of tasks) {
    if (isActiveState(task.state)) {
      activeCount += 1;
    }
    if (hasUnknownTotal(task)) {
      unknownTotalCount += 1;
    }
  }
  return { itemCount: tasks.length, activeCount, unknownTotalCount };
}

/**
 * What the shell may say after `request_cancel`.
 *
 * Only `requested` / `already_requested` are recorded as "cancellation
 * request recorded": the contract says the task turns `cancelled` only at a
 * safe stop, so nothing here may claim the task has stopped.
 */
export type CancelFeedback =
  | { status: "requested" }
  | { status: "rejected"; reason: string };

export function feedbackFromDecision(decision: CancelDecision): CancelFeedback {
  switch (decision) {
    case "requested":
    case "already_requested":
      return { status: "requested" };
    case "not_cancellable":
      return { status: "rejected", reason: uiLabels.taskCancelRejectedNotCancellable };
    case "stale_generation":
      return { status: "rejected", reason: uiLabels.taskCancelRejectedStale };
    case "not_found":
      return { status: "rejected", reason: uiLabels.taskCancelRejectedMissing };
  }
}
