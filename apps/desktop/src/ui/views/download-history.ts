import type { Task } from "../../contract";
import type { DownloadPresentation } from "./download-presentation";

export type DownloadHistoryGroup = "completed" | "unfinished";
export interface ArchivedDownload {
  presentation: DownloadPresentation;
  group: DownloadHistoryGroup;
  status: string;
}

/** Persisted identity alone does not prove success or an active local task. */
export function archivedDownloadStatus(entry: DownloadPresentation): string {
  const history = entry.history;
  if (!history) return "状态未记录";
  if (history.state === "succeeded") return "已完成";
  if (history.state === "failed") return "下载失败";
  if (history.state === "cancelled") return "已取消";
  if (history.activeElsewhere) return "在另一窗口中进行";
  if (history.interrupted) return "下载已中断";
  if (history.state === "paused") return "已暂停";
  if (history.state === "waiting_user") return "等待处理";
  return "等待恢复";
}

/** O(tasks + entries). Live identity wins across all attempt generations.
 * History stays read-only and never creates a synthetic cancellable Task.
 */
export function archivedDownloads(entries: readonly DownloadPresentation[], tasks: readonly Task[]): ArchivedDownload[] {
  const live = new Map<string, Set<string>>();
  for (const task of tasks) {
    const ids = live.get(task.operationId) ?? new Set<string>();
    ids.add(task.id);
    live.set(task.operationId, ids);
  }
  const seen = new Map<string, Set<string>>();
  const result: ArchivedDownload[] = [];
  for (const entry of entries) {
    if (live.get(entry.operationId)?.has(entry.taskId)) continue;
    const ids = seen.get(entry.operationId) ?? new Set<string>();
    if (ids.has(entry.taskId)) continue;
    ids.add(entry.taskId);
    seen.set(entry.operationId, ids);
    result.push({ presentation: entry,
      group: entry.history?.state === "succeeded" ? "completed" : "unfinished",
      status: archivedDownloadStatus(entry) });
  }
  return result;
}
