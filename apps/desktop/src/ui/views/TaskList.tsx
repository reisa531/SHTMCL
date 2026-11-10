import { memo, useMemo, useState, type CSSProperties } from "react";
import { isTauriRuntime, openDownloadDestination } from "../../bridge";

import type { Task } from "../../contract";
import { shellCopy } from "../../ux/copy";
import { Button } from "../components/Button";
import { InlineError } from "../components/InlineError";
import { List, ListRow } from "../components/List";
import { Progress } from "../components/Progress";
import { StatusPill } from "../components/StatusPill";
import { Icon, type IconName } from "../Icons";
import { uiLabels } from "../labels";
import { RetryCachedDownload, RetryInstanceInstall } from "./RetryCachedDownload";
import { canCancel, hasUnknownTotal, taskPhaseLabel, type CancelFeedback } from "./task-format";
import { downloadDestinationLabel, downloadPhaseLabel, type DownloadPresentation } from "./download-presentation";

const EMPTY_PRESENTATIONS: readonly DownloadPresentation[] = [];
const RESOURCE_ICONS: Record<DownloadPresentation["resourceType"], IconName> = {
  game_environment: "download",
  loader: "layers",
  mod: "layers",
  modpack: "layers",
  shader: "cloud",
  resourcepack: "cloud",
  datapack: "layers",
};

export interface TaskListProps {
  tasks: readonly Task[];
  presentations?: readonly DownloadPresentation[];
  relatedTasks?: readonly Task[];
  /** Per-task outcome of the last `request_cancel` call. */
  feedback: Readonly<Record<string, CancelFeedback>>;
  /** Task ids with a cancel request in flight. */
  pendingCancel: ReadonlySet<string>;
  onCancel: (task: Task) => void;
  onRefresh: () => void;
  onViewInstance?: (id: string) => void;
  onRetryAccepted?: () => void;
}

export interface TaskRowProps {
  task: Task;
  presentation?: DownloadPresentation;
  relatedTasks?: readonly Task[];
  feedback: CancelFeedback | undefined;
  pending: boolean;
  onCancel: (task: Task) => void;
  onRefresh: () => void;
  onViewInstance?: (id: string) => void;
  onRetryAccepted?: () => void;
}

function DownloadProgress({ task, name }: { task: Task; name: string }) {
  if (!task.progress) return null;
  const total = task.progress.total;
  const percent = total !== null && total > 0
    ? Math.min(100, Math.max(0, Math.round(task.progress.current / total * 100)))
    : null;
  return <div className="progress">
    <div className="progress__track" role="progressbar" aria-label={`${name}进度`}
      aria-valuemin={0} aria-valuemax={100} aria-valuenow={percent ?? undefined}
      aria-valuetext={percent === null ? "处理中" : `${percent}%`}
      data-indeterminate={percent === null ? "true" : undefined}
      style={percent === null ? undefined : { "--progress-value": `${percent}%` } as CSSProperties}>
      <span className="progress__fill" />
    </div>
    <span className="progress__text">{percent === null ? "处理中" : `${percent}%`}</span>
  </div>;
}

function sameProgress(a: Task["progress"], b: Task["progress"]): boolean {
  if (a === undefined || b === undefined) {
    return a === b;
  }
  return a.current === b.current && a.total === b.total && a.unit === b.unit;
}

function sameFeedback(
  a: CancelFeedback | undefined,
  b: CancelFeedback | undefined,
): boolean {
  if (a === undefined || b === undefined) {
    return a === b;
  }
  if (a.status !== b.status) {
    return false;
  }
  return a.status === "rejected" && b.status === "rejected"
    ? a.reason === b.reason
    : true;
}

/**
 * Explicit memo comparator for a task row (evaluator defect D1).
 *
 * React's default shallow compare fails as soon as the parent hands over fresh
 * objects or fresh closures — which is exactly what a list render does — so the
 * comparison is written in terms of what the row actually renders:
 * `id / state / phase / attemptGeneration / cancelRequested / progress /
 * error.code / error.retryable`, plus the two render-only inputs (`pending`,
 * `feedback`).
 *
 * Cancel and refresh callbacks are not compared: the shell passes stable callbacks
 * (`useCallback`) and both are called with the row's own task, so a new function
 * identity cannot change what this row displays. If a handler ever needs to
 * capture per-render state, it must be compared here as well. Instance navigation
 * is compared explicitly because callers can change its destination context.
 */
export function taskRowPropsAreEqual(
  prev: TaskRowProps,
  next: TaskRowProps,
): boolean {
  if (prev.pending !== next.pending) {
    return false;
  }
  if (prev.presentation !== next.presentation) return false;
  if (prev.onViewInstance !== next.onViewInstance) return false;
  if (prev.onRetryAccepted !== next.onRetryAccepted) return false;
  const before = prev.relatedTasks ?? [], after = next.relatedTasks ?? [];
  if (before.length !== after.length || before.some((task, index) => {
    const other = after[index];
    return task.id !== other.id || task.phase !== other.phase || task.state !== other.state;
  })) return false;
  if (!sameFeedback(prev.feedback, next.feedback)) {
    return false;
  }

  const a = prev.task;
  const b = next.task;
  return (
    a.id === b.id &&
    a.operationId === b.operationId &&
    a.state === b.state &&
    a.phase === b.phase &&
    a.attemptGeneration === b.attemptGeneration &&
    a.cancelRequested === b.cancelRequested &&
    sameProgress(a.progress, b.progress) &&
    (a.error?.code ?? null) === (b.error?.code ?? null) &&
    (a.error?.retryable ?? false) === (b.error?.retryable ?? false)
  );
}

/**
 * One row per task, keyed by `task.id`. Memoised with the comparator above so a
 * single `task.*` event re-renders only the row it names.
 *
 * Exported (with its comparator) so the memo boundary can be asserted in
 * `tests/modules/10` without a DOM renderer.
 */
export const TaskRow = memo(function TaskRow({
  task,
  presentation,
  relatedTasks,
  feedback,
  pending,
  onCancel,
  onRefresh,
  onViewInstance,
  onRetryAccepted,
}: TaskRowProps) {
  const errorId = `task-${task.id}-cancel`;
  const rejected = feedback?.status === "rejected" ? feedback : undefined;
  const requested = feedback?.status === "requested";
  const [destinationFeedback, setDestinationFeedback] = useState("");
  const [opening, setOpening] = useState(false);
  const instanceId = task.state === "succeeded" && presentation?.destination.kind === "instance"
    ? presentation.destination.instanceId : undefined;
  async function openDestination() {
    if (opening) return;
    setOpening(true); setDestinationFeedback("");
    try { await openDownloadDestination(task.operationId, task.id); }
    catch { setDestinationFeedback("目录暂时无法打开，请确认文件仍在原位置。"); }
    finally { setOpening(false); }
  }
  async function copyDestination() {
    if (!presentation?.destination.path) return;
    try { await navigator.clipboard.writeText(presentation.destination.path); setDestinationFeedback("路径已复制"); }
    catch { setDestinationFeedback("路径未复制，请重试。"); }
  }

  return (
    <ListRow id={`task-${task.id}`}>
      <div className="task__head">
        <div className="task__identity">
          <span className="task__resource-icon">
            <Icon name={presentation ? RESOURCE_ICONS[presentation.resourceType] : "download"} size={23} />
          </span>
          <span className="task__id task__name">
            {presentation ? `${presentation.name}${presentation.version ? ` · ${presentation.version}` : ""}` : "后台任务"}
          </span>
        </div>
        <StatusPill state={task.state} />
      </div>

      {presentation ? (
        <>
          <p className="task__phase">{downloadDestinationLabel(presentation)}</p>
          {presentation.destination.path ? (
            <p className="task__note">位置：<span className="download-destination" title={presentation.destination.path}>{presentation.destination.path}</span></p>
          ) : null}
          {presentation.resourceType === "mod" && presentation.destination.kind === "cache" ? <p className="task__note">仅下载 · 未添加到实例</p> : null}
          {canCancel(task) ? (
            <p className="task__note">{downloadPhaseLabel(task.phase)}</p>
          ) : null}
          <DownloadProgress task={task} name={presentation.name} />
          {task.state === "failed" ? <p className="task__error">任务未完成，详情中可查看原因。</p> : null}
        </>
      ) : null}

      {presentation ? <details className="task__details"><summary>技术详情</summary>
        <p>任务：{task.id}</p>
        <p>阶段：{task.phase}</p>
        {task.progress ? <p>进度：{task.progress.current} / {task.progress.total ?? "未知"} {task.progress.unit}</p> : null}
        {task.error ? <p>{task.error.code}</p> : null}
        {relatedTasks?.length ? <div className="task__children">
          <p>{relatedTasks.length} 项关联任务</p>
          <ul>{relatedTasks.map((child) => <li key={child.id}>{child.id} · {taskPhaseLabel(child.phase)} · <StatusPill state={child.state} /></li>)}</ul>
        </div> : null}
      </details> : (
        <>
      <p className="task__phase">{taskPhaseLabel(task.phase)}</p>

      {task.progress ? (
        <Progress
          label={uiLabels.progressLabel}
          current={task.progress.current}
          total={task.progress.total}
          unit={task.progress.unit}
        />
      ) : null}

      {hasUnknownTotal(task) ? (
        <p className="task__note">{uiLabels.progressUnknownTotal}</p>
      ) : null}

      {task.error ? (
        <details className="task__details"><summary>技术详情</summary>
        <p className="task__error">
          <span className="task__error-code">{task.error.code}</span>
          {task.error.retryable ? (
            <span className="task__error-flag">{uiLabels.taskRetryable}</span>
          ) : null}
        </p>
        <p>任务：{task.id}</p>
        </details>
      ) : null}
        </>
      )}

      <div className="task__foot">
        {presentation ? <RetryCachedDownload entry={presentation} task={task} onAccepted={onRetryAccepted} /> : null}
        {presentation ? <RetryInstanceInstall entry={presentation} task={task} onAccepted={onRetryAccepted} /> : null}
        {instanceId && onViewInstance ? <Button size="sm" onClick={() => onViewInstance(instanceId)}>查看实例</Button> : null}
        {presentation && presentation.destination.kind !== "file" && isTauriRuntime() ? <Button size="sm" onClick={() => void openDestination()} pending={opening} pendingLabel="正在打开">{presentation.destination.kind === "cache" ? "打开缓存目录" : "打开实例目录"}</Button> : null}
        {presentation?.destination.path ? <Button size="sm" variant="quiet" onClick={() => void copyDestination()}>复制路径</Button> : null}
        {canCancel(task) ? (
          <Button
            variant="secondary"
            size="sm"
            icon="stop"
            pending={pending}
            pendingLabel={uiLabels.taskCancelPending}
            onClick={() => onCancel(task)}
            aria-describedby={rejected ? errorId : undefined}
            data-attempt-generation={task.attemptGeneration}
          >
            {uiLabels.taskCancel}
          </Button>
        ) : null}

        {task.cancelRequested || requested ? (
          <p className="task__note" aria-live="polite">
            {shellCopy.cancelPending}
          </p>
        ) : null}
      </div>
      {destinationFeedback ? <p className="task__note" role="status">{destinationFeedback}</p> : null}

      {rejected ? (
        <InlineError id={errorId} message={rejected.reason}>
          <Button variant="quiet" size="sm" icon="refresh" onClick={onRefresh}>
            {uiLabels.taskRefresh}
          </Button>
        </InlineError>
      ) : null}
    </ListRow>
  );
}, taskRowPropsAreEqual);

/**
 * Row-level props for one task. Exported so the memo decision can be simulated
 * against real reconciler inputs in `tests/modules/10/memo.test.mjs`.
 */
export function taskRowPropsFor(
  task: Task,
  feedback: Readonly<Record<string, CancelFeedback>>,
  pendingCancel: ReadonlySet<string>,
  onCancel: (task: Task) => void,
  onRefresh: () => void,
): TaskRowProps {
  return {
    task,
    feedback: feedback[task.id],
    pending: pendingCancel.has(task.id),
    onCancel,
    onRefresh,
  };
}

export function TaskList({
  tasks,
  presentations = EMPTY_PRESENTATIONS,
  relatedTasks,
  feedback,
  pendingCancel,
  onCancel,
  onRefresh,
  onViewInstance,
  onRetryAccepted,
}: TaskListProps) {
  const byOperation = useMemo(() => {
    const index = new Map<string, Map<string, DownloadPresentation>>();
    for (const entry of presentations) {
      const tasks = index.get(entry.operationId) ?? new Map<string, DownloadPresentation>();
      tasks.set(entry.taskId, entry);
      index.set(entry.operationId, tasks);
    }
    return index;
  }, [presentations]);
  const children = useMemo(() => {
    const index = new Map<string, Task[]>();
    for (const task of relatedTasks ?? []) {
      const members = index.get(task.operationId) ?? [];
      members.push(task);
      index.set(task.operationId, members);
    }
    return index;
  }, [relatedTasks]);
  return (
    <List label={uiLabels.tasksHeading}>
      {tasks.map((task) => (
        <TaskRow
          key={task.id}
          {...taskRowPropsFor(task, feedback, pendingCancel, onCancel, onRefresh)}
          onViewInstance={onViewInstance}
          onRetryAccepted={onRetryAccepted}
          presentation={byOperation.get(task.operationId)?.get(task.id)}
          relatedTasks={children.get(task.operationId)?.filter((child) => child.id !== task.id)}
        />
      ))}
    </List>
  );
}
