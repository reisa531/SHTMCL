import { useEffect, useRef, useState } from "react";
import type { Task } from "../../contract";
import { Button } from "../components/Button";
import type { DownloadPresentation } from "./download-presentation";
import { environmentApi } from "./environment-api";
import { retryInstallation } from "./install-recovery-api";

/** A fresh cache request reuses verified objects; it never recreates an instance. */
export function canRetryCachedDownload(entry: DownloadPresentation, task?: Task): boolean {
  if (entry.resourceType !== "game_environment" || entry.destination.kind !== "cache" || !entry.version?.trim()) return false;
  return stopped(entry, task);
}

export function canRetryInstanceInstall(entry: DownloadPresentation, task?: Task): boolean {
  return entry.resourceType === "game_environment" && entry.destination.kind === "instance"
    && Boolean(entry.destination.instanceId) && entry.canRetryInstall === true && stopped(entry, task);
}

function stopped(entry: DownloadPresentation, task?: Task): boolean {
  if (task) {
    return task.id === entry.taskId && task.operationId === entry.operationId
      && (task.state === "failed" || task.state === "cancelled");
  }
  const history = entry.history;
  if (!history || history.activeElsewhere) return false;
  return history.state === "failed" || history.state === "cancelled"
    || (history.interrupted === true && (history.state === "queued" || history.state === "running"));
}

interface Props {
  entry: DownloadPresentation;
  task?: Task;
  onAccepted?: () => void;
}

export function RetryCachedDownload({ entry, task, onAccepted }: Props) {
  if (!canRetryCachedDownload(entry, task)) return null;
  // A new record/version gets fresh state and invalidates any previous response.
  return <RetryAction key={JSON.stringify([entry.operationId, entry.taskId, entry.version])}
    perform={() => environmentApi().download(entry.version!)} label="重新下载" onAccepted={onAccepted} />;
}

export function RetryInstanceInstall({ entry, task, onAccepted }: Props) {
  if (!canRetryInstanceInstall(entry, task)) return null;
  return <RetryAction key={JSON.stringify([entry.operationId, entry.taskId])}
    perform={() => retryInstallation(entry.operationId, entry.taskId)} label="重新安装" onAccepted={onAccepted} />;
}

function RetryAction({ perform, label, onAccepted }: { perform: () => Promise<string>; label: string; onAccepted?: () => void }) {
  const [pending, setPending] = useState(false);
  const [submitted, setSubmitted] = useState(false);
  const [error, setError] = useState("");
  const busy = useRef(false);
  const mounted = useRef(false);
  const accepted = useRef(onAccepted);
  accepted.current = onAccepted;
  useEffect(() => { mounted.current = true; return () => { mounted.current = false; }; }, []);

  async function retry() {
    if (busy.current) return;
    busy.current = true;
    setPending(true);
    setError("");
    try {
      await perform();
    } catch (failure) {
      if (!mounted.current) return;
      busy.current = false;
      setPending(false);
      const code = failure && typeof failure === "object" ? (failure as {code?: string}).code : undefined;
      setError(code === "launch.install.busy" ? "正在处理另一个任务，请稍后重试。"
        : code === "launch.install.target_exists" ? "实例已存在，可以在实例页查看。"
        : code === "launch.install.recovery_unavailable" ? "安装记录不可用，请重新选择版本和组件。"
        : label === "重新安装" ? "未能开始安装，请重试。" : "未能开始下载，请重试。");
      return;
    }
    if (!mounted.current) return;
    setPending(false);
    setSubmitted(true);
    accepted.current?.();
  }

  return <>
    <Button size="sm" icon="refresh" pending={pending} pendingLabel="正在加入"
      disabled={submitted} onClick={() => void retry()}>{submitted ? "已加入下载" : label}</Button>
    {error ? <p className="task__note" role="status">{error}</p> : null}
  </>;
}
