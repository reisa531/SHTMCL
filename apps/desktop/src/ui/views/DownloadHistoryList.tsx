import { useMemo, useState } from "react";
import { isTauriRuntime, openDownloadDestination } from "../../bridge";
import type { Task } from "../../contract";
import { Button } from "../components/Button";
import { List, ListRow } from "../components/List";
import { Icon } from "../Icons";
import { RetryCachedDownload, RetryInstanceInstall } from "./RetryCachedDownload";
import { archivedDownloads, type ArchivedDownload, type DownloadHistoryGroup } from "./download-history";
import { downloadDestinationLabel, type DownloadPresentation } from "./download-presentation";

export interface DownloadHistoryListProps {
  entries: readonly DownloadPresentation[];
  tasks: readonly Task[];
  group: DownloadHistoryGroup;
  onViewInstance?: (id: string) => void;
  onRetryAccepted?: () => void;
}

function DownloadHistoryRow({ record, onViewInstance, onRetryAccepted }: { record: ArchivedDownload; onViewInstance?: (id: string) => void; onRetryAccepted?: () => void }) {
  const { presentation: entry, status } = record;
  const [feedback, setFeedback] = useState("");
  const [opening, setOpening] = useState(false);
  const instanceId = record.group === "completed" && entry.destination.kind === "instance"
    ? entry.destination.instanceId : undefined;
  const tone = record.group === "completed" ? "ok" : entry.history?.state === "failed" ? "error" : "idle";
  async function openDestination() {
    if (opening) return;
    setOpening(true); setFeedback("");
    try { await openDownloadDestination(entry.operationId, entry.taskId); }
    catch { setFeedback("目录暂时无法打开，请确认文件仍在原位置。"); }
    finally { setOpening(false); }
  }
  async function copyDestination() {
    if (!entry.destination.path) return;
    try { await navigator.clipboard.writeText(entry.destination.path); setFeedback("路径已复制"); }
    catch { setFeedback("路径未复制，请重试。"); }
  }
  return <ListRow>
    <div className="task__head">
      <div className="task__identity">
        <span className="task__resource-icon"><Icon name={entry.resourceType === "game_environment" ? "download" : "layers"} size={23} /></span>
        <span className="task__id task__name">{entry.name}{entry.version ? ` · ${entry.version}` : ""}</span>
      </div>
      <span className={`status-pill status-pill--${tone}`}>{status}</span>
    </div>
    <p className="task__phase">{downloadDestinationLabel(entry)}</p>
    {entry.destination.path ? <p className="task__note">位置：<span className="download-destination" title={entry.destination.path}>{entry.destination.path}</span></p> : null}
    {entry.resourceType === "mod" && entry.destination.kind === "cache" ? <p className="task__note">仅下载 · 未添加到实例</p> : null}
    <details className="task__details"><summary>技术详情</summary>
      <p>任务：{entry.taskId}</p>
      {entry.history ? <><p>阶段：{entry.history.phase}</p><p>尝试：{entry.history.attemptGeneration}</p></> : null}
      {entry.history?.errorCode ? <p>{entry.history.errorCode}</p> : null}
    </details>
    <div className="task__foot">
      <RetryCachedDownload entry={entry} onAccepted={onRetryAccepted} />
      <RetryInstanceInstall entry={entry} onAccepted={onRetryAccepted} />
      {instanceId && onViewInstance ? <Button size="sm" onClick={() => onViewInstance(instanceId)}>查看实例</Button> : null}
      {entry.destination.kind !== "file" && isTauriRuntime() ? <Button size="sm" pending={opening} pendingLabel="正在打开" onClick={() => void openDestination()}>{entry.destination.kind === "cache" ? "打开缓存目录" : "打开实例目录"}</Button> : null}
      {entry.destination.path ? <Button size="sm" variant="quiet" onClick={() => void copyDestination()}>复制路径</Button> : null}
    </div>
    {feedback ? <p className="task__note" role="status">{feedback}</p> : null}
  </ListRow>;
}

/** Retained history stays read-only; retry creates a separate cache task. */
export function DownloadHistoryList({ entries, tasks, group, onViewInstance, onRetryAccepted }: DownloadHistoryListProps) {
  const records = useMemo(() => archivedDownloads(entries, tasks).filter((record) => record.group === group), [entries, tasks, group]);
  if (records.length === 0) return null;
  return <List label={group === "completed" ? "已完成的下载记录" : "未完成的下载记录"}>
    {records.map((record) => <DownloadHistoryRow key={JSON.stringify([record.presentation.operationId, record.presentation.taskId])} record={record} onViewInstance={onViewInstance} onRetryAccepted={onRetryAccepted} />)}
  </List>;
}
