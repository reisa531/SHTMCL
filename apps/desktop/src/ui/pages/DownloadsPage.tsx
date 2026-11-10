import { useCallback, useMemo, useState } from "react";

import type { Task } from "../../contract";
import { copy, pageCopy } from "../../ux/copy";
import { EmptyState } from "../components/EmptyState";
import { TabPanel, Tabs } from "../components/Tabs";
import { uiLabels, formatTaskCount } from "../labels";
import { PageHeader } from "../views/PageHeader";
import { PageStateView } from "../views/PageState";
import { selectPageState, type SnapshotPhase } from "../views/page-state";
import { TaskList } from "../views/TaskList";
import type { DownloadPresentation } from "../views/download-presentation";
import { downloadOperations } from "../views/download-operations";
import { summarizeTasks, type CancelFeedback } from "../views/task-format";
import type { MotionPoint } from "../components/motion";
import { archivedDownloads } from "../views/download-history";
import { DownloadHistoryList } from "../views/DownloadHistoryList";

export type DownloadTab = "active" | "completed" | "unfinished";

const TAB_ITEMS = [
  { id: "active", label: copy.downloads.activeTab },
  { id: "completed", label: copy.downloads.completedTab },
  { id: "unfinished", label: "未完成" },
] as const satisfies ReadonlyArray<{ id: DownloadTab; label: string }>;
const PANEL_ID = "downloads-panel";

export interface DownloadsPageProps {
  /** Snapshot baseline plus live `task.*` events, owned by the shell. */
  tasks: readonly Task[];
  presentations?: readonly DownloadPresentation[];
  /** False until the first snapshot lands, so the count is not invented. */
  hasSnapshot: boolean;
  phase: SnapshotPhase;
  refreshing: boolean;
  feedback: Readonly<Record<string, CancelFeedback>>;
  pendingCancel: ReadonlySet<string>;
  onCancel: (task: Task) => void;
  onRefresh: () => void;
  onAccepted?: (origin?: MotionPoint) => void;
  onViewInstance?: (id: string) => void;
}

/**
 * Tasks come from the shell's task stream: the `get_shell_snapshot` baseline
 * advanced by live `task.*` events. There is no synthetic row. Cancelling goes
 * through the shell's `request_cancel` call, and the page only ever reports
 * that the request was recorded, never that the task stopped.
 */
export function DownloadsPage({
  tasks,
  presentations,
  hasSnapshot,
  phase,
  refreshing,
  feedback,
  pendingCancel,
  onCancel,
  onRefresh,
  onAccepted,
  onViewInstance,
}: DownloadsPageProps) {
  const [tab, setTab] = useState<DownloadTab>("active");
  const retryAccepted = useCallback(() => {
    setTab("active");
    onAccepted?.();
  }, [onAccepted]);

  const operations = useMemo(() => downloadOperations(tasks, presentations ?? []), [tasks, presentations]);
  const roots = useMemo(() => operations.map((operation) => operation.task), [operations]);
  const summary = useMemo(() => summarizeTasks(roots), [roots]);
  const active = useMemo(
    () => operations.filter((operation) => operation.group === "active"),
    [operations],
  );
  const completed = useMemo(
    () => operations.filter((operation) => operation.group === "completed"),
    [operations],
  );
  const unfinished = operations.filter((operation) => operation.group === "unfinished");
  const archived = useMemo(() => archivedDownloads(presentations ?? [], tasks), [presentations, tasks]);
  const archivedVisible = archived.filter((record) => record.group === tab);
  const completedCount = completed.length + archived.filter((record) => record.group === "completed").length;
  const unfinishedCount = unfinished.length + archived.filter((record) => record.group === "unfinished").length;

  const state = selectPageState({
    phase: phase === "loading" && operations.length > 0 ? "ready" : phase,
    hasDataSource: true, ...summary, itemCount: operations.length + archived.length, activeCount: active.length,
  });
  const visible = tab === "active" ? active : tab === "completed" ? completed : unfinished;
  const empty =
    tab === "active"
      ? { title: copy.downloads.activeTitle, body: copy.downloads.activeBody }
      : tab === "unfinished" ? { title: "没有未完成的下载", body: "暂停、等待处理、失败或取消的下载会出现在这里。" } : {
          title: copy.downloads.completedTitle,
          body: copy.downloads.completedBody,
        };

  return (
    <>
      <PageHeader
        icon="download"
        title={pageCopy.tasks.title}
        description="查看正在获取的内容，以及它们将保存到哪里。"
        actions={
          hasSnapshot ? (
            <span className="count-note">{formatTaskCount(operations.length + archived.length)}</span>
          ) : undefined
        }
      />
      <PageStateView
        state={state === "busy" || state === "partial" ? "success" : state}
        loadingLabel={uiLabels.loadingTitle}
        onRetry={onRefresh}
        retryPending={refreshing}
        preserveContent={operations.length + archived.length > 0}
      >
        <Tabs
          items={TAB_ITEMS.map((item) => ({ ...item, label: hasSnapshot ? `${item.label} · ${{ active: active.length, completed: completedCount, unfinished: unfinishedCount }[item.id]}` : item.label }))}
          active={tab}
          onChange={setTab}
          label="下载视图"
          panelId={PANEL_ID}
        />

        <TabPanel id={PANEL_ID} activeTabId={tab}>
          {visible.length === 0 && archivedVisible.length === 0 ? (
            <EmptyState icon="download" title={empty.title} body={empty.body} />
          ) : <>
            {visible.length > 0 ? (
            <TaskList
              tasks={visible.map((operation) => operation.task)}
              relatedTasks={tasks}
              presentations={presentations}
              feedback={feedback}
              pendingCancel={pendingCancel}
              onCancel={onCancel}
              onRefresh={onRefresh}
              onViewInstance={onViewInstance}
              onRetryAccepted={retryAccepted}
            />
            ) : null}
            {tab !== "active" ? <DownloadHistoryList entries={presentations ?? []} tasks={tasks} group={tab} onViewInstance={onViewInstance} onRetryAccepted={retryAccepted} /> : null}
          </>}
        </TabPanel>
      </PageStateView>
    </>
  );
}
