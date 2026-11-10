import type { Task } from "../../contract";
import type { DownloadPresentation } from "./download-presentation";

export type DownloadGroup = "active" | "completed" | "unfinished";
export interface DownloadOperation {
  task: Task;
  presentation: DownloadPresentation;
  children: readonly Task[];
  group: DownloadGroup;
}

export function downloadGroup(task: Task): DownloadGroup {
  if (task.state === "succeeded") return "completed";
  return task.state === "queued" || task.state === "running" ? "active" : "unfinished";
}

/** Only the use-case's identified parent owns the result, never a file child.
 * A missing parent is pending snapshot recovery, not evidence of completion.
 * Generations are comparable within a task ID only. This is O(tasks + entries).
 */
export function downloadOperations(tasks: readonly Task[], entries: readonly DownloadPresentation[]): DownloadOperation[] {
  const byOperation = new Map<string, Map<string, Task>>();
  for (const task of tasks) {
    let members = byOperation.get(task.operationId);
    if (!members) { members = new Map(); byOperation.set(task.operationId, members); }
    const previous = members.get(task.id);
    if (!previous || previous.attemptGeneration < task.attemptGeneration) members.set(task.id, task);
  }
  const seen = new Set<string>();
  const result: DownloadOperation[] = [];
  for (const entry of entries) {
    if (seen.has(entry.operationId)) continue;
    const members = byOperation.get(entry.operationId);
    const task = members?.get(entry.taskId);
    if (!task || !members) continue;
    seen.add(entry.operationId);
    result.push({ task, presentation: entry,
      children: [...members.values()].filter((member) => member.id !== task.id), group: downloadGroup(task) });
  }
  return result;
}
