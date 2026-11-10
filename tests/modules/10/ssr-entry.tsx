// Static-render fixtures for module 10.
//
// These exist only to exercise the components with real contract shapes; they
// are not shipped data. Every task object below is a valid `Task` per
// `contracts/domain-v1.md` §4, and the rendered output is asserted against the
// same wording rules the product copy follows.

import { renderToStaticMarkup } from "react-dom/server";
export { modrinthApi } from "../../../apps/desktop/src/ui/views/modrinth-api";
export { resourceCatalogApi, resourceSearchError } from "../../../apps/desktop/src/ui/views/resource-catalog-api";

import {
  EVENT_ID_MEMORY,
  INITIAL_TASK_STREAM,
  reduceTaskStream,
} from "../../../apps/desktop/src/ui/views/task-stream";
import { App } from "../../../apps/desktop/src/App";
import type { EventEnvelope, Task } from "../../../apps/desktop/src/contract";
import { navLabels, pageCopy, primaryNav } from "../../../apps/desktop/src/ux/copy";
import type { PageId } from "../../../apps/desktop/src/ux/copy";
import { Button } from "../../../apps/desktop/src/ui/components/Button";
import { InstanceDetails } from "../../../apps/desktop/src/ui/components/InstanceDetails";
import { InstanceManagementPanel } from "../../../apps/desktop/src/ui/components/InstanceManagementPanel";
export { instanceCopyApi, instanceCopyError, encodeInstanceReference, parseInstanceReference } from "../../../apps/desktop/src/ui/views/instance-copy-api";
export { fabricApi } from "../../../apps/desktop/src/ui/views/fabric-api";
import { InstanceArtwork } from "../../../apps/desktop/src/ui/components/InstanceArtwork";
import type { InstalledGame } from "../../../apps/desktop/src/ui/views/game-install-api";
import { WindowControls } from "../../../apps/desktop/src/ui/components/WindowControls";
import { Banner } from "../../../apps/desktop/src/ui/components/Banner";
import { AccountDialog, LocalIdentityForm, type LocalIdentityFormProps } from "../../../apps/desktop/src/ui/views/AccountDialog";
import { DiscoverPage } from "../../../apps/desktop/src/ui/pages/DiscoverPage";
import { DownloadHubPage } from "../../../apps/desktop/src/ui/pages/DownloadHubPage";
import { MorePage } from "../../../apps/desktop/src/ui/pages/MorePage";
import { DownloadsPage } from "../../../apps/desktop/src/ui/pages/DownloadsPage";
import { DownloadHistoryList } from "../../../apps/desktop/src/ui/views/DownloadHistoryList";
import { EnvironmentLibrary } from "../../../apps/desktop/src/ui/views/EnvironmentLibrary";
import { InstancesPage } from "../../../apps/desktop/src/ui/pages/InstancesPage";
import { CreateInstancePage } from "../../../apps/desktop/src/ui/pages/CreateInstancePage";
import type { DownloadPresentation } from "../../../apps/desktop/src/ui/views/download-presentation";
import { MultiplayerPage } from "../../../apps/desktop/src/ui/pages/MultiplayerPage";
import { SettingsPage } from "../../../apps/desktop/src/ui/pages/SettingsPage";
import { StartPage } from "../../../apps/desktop/src/ui/pages/StartPage";
import { PageStateView } from "../../../apps/desktop/src/ui/views/PageState";
import { Sidebar } from "../../../apps/desktop/src/ui/views/Sidebar";
import { TaskList, TaskRow } from "../../../apps/desktop/src/ui/views/TaskList";
import { TopBar } from "../../../apps/desktop/src/ui/views/TopBar";
import type { CancelFeedback } from "../../../apps/desktop/src/ui/views/task-format";
import type { SnapshotPhase } from "../../../apps/desktop/src/ui/views/page-state";
import {
  DEFAULT_PREFERENCES,
  type ShellPreferences,
} from "../../../apps/desktop/src/ux/preferences";

export function renderWindowControls(native: boolean) {
  return renderToStaticMarkup(<WindowControls native={native} />);
}
export function renderInstanceDetails(game: InstalledGame) {
  return renderToStaticMarkup(<div><InstanceArtwork loader={game.version.loader} /><InstanceDetails game={game} /></div>);
}
export function renderInstanceManagement(game: InstalledGame, initialTab: "overview" | "settings" | "mods" = "overview") {
  return renderToStaticMarkup(<InstanceManagementPanel game={game} targetPath={`instances/${game.id}`} initialTab={initialTab} />);
}

export {
  CLOSED_OVERLAY,
  focusAfterClose,
  reduceOverlay,
} from "../../../apps/desktop/src/ui/components/overlay";
export {
  progressPercent,
  progressText,
} from "../../../apps/desktop/src/ui/components/Progress";
export { selectPageState } from "../../../apps/desktop/src/ui/views/page-state";
export {
  canCancel,
  feedbackFromDecision,
  isTerminalState,
  summarizeTasks,
} from "../../../apps/desktop/src/ui/views/task-format";
export {
  taskRowPropsAreEqual,
  taskRowPropsFor,
} from "../../../apps/desktop/src/ui/views/TaskList";

export { navLabels, pageCopy, primaryNav };
export * from "../../../apps/desktop/src/ui/views/download-presentation";
export * from "../../../apps/desktop/src/ui/views/download-operations";
export * from "../../../apps/desktop/src/ui/views/download-history";
export * from "../../../apps/desktop/src/ui/views/environment-api";
export function renderEnvironmentLibrary(): string { return renderToStaticMarkup(<EnvironmentLibrary revision="" onCreate={() => {}} onGetGame={() => {}} />); }
export function renderDownloadHistory(entries: readonly DownloadPresentation[], tasks: readonly Task[], group: "completed" | "unfinished"): string {
  return renderToStaticMarkup(<DownloadHistoryList entries={entries} tasks={tasks} group={group} />);
}
export * from "../../../apps/desktop/src/ui/components/motion";
export function renderDownloadBadge(count: number): string {
  return renderToStaticMarkup(<Sidebar page="downloads" onNavigate={() => {}} appInfo={null} environment="browser" activeDownloads={count} />);
}
export function renderCreationPage(): string {
  return renderToStaticMarkup(<CreateInstancePage onBack={() => {}} onAccepted={() => {}} />);
}
export function renderInstancesWithCreate(): string {
  return renderToStaticMarkup(<InstancesPage phase="ready" retrying={false} onRetry={() => {}} onCreate={() => {}} />);
}
export function renderPresentedTasks(tasks: readonly Task[], presentations: readonly DownloadPresentation[]): string {
  return renderToStaticMarkup(<TaskList tasks={tasks} presentations={presentations} feedback={{}} pendingCancel={new Set<string>()} onCancel={() => {}} onRefresh={() => {}} />);
}
export { gameInstallApi, installError } from "../../../apps/desktop/src/ui/views/game-install-api";
export { instanceSettingsApi, instanceSettingsError } from "../../../apps/desktop/src/ui/views/instance-settings-api";
export { modMetadataApi, parseModMetadata } from "../../../apps/desktop/src/ui/views/mod-metadata-api";
export { modToggleApi, modToggleError } from "../../../apps/desktop/src/ui/views/mod-toggle-api";
export { gameLaunchApi, launchError } from "../../../apps/desktop/src/ui/views/game-launch-api";
export function renderLocalIdentityForm(options: Pick<LocalIdentityFormProps, "username" | "pending" | "connected">): string {
  return renderToStaticMarkup(<LocalIdentityForm {...options} onUsernameChange={() => {}} onSubmit={() => {}} />);
}
export type { PageId, SnapshotPhase, CancelFeedback };

export { EVENT_ID_MEMORY, INITIAL_TASK_STREAM, reduceTaskStream };

/** Builds a contract-shaped event envelope for stream assertions. */
export function makeEvent(init: {
  eventId: string;
  sequence: number;
  attemptGeneration: number;
  kind: string;
  payload: unknown;
  operationId?: string;
  revision?: number;
  entityId?: string;
}): EventEnvelope {
  return {
    eventId: init.eventId,
    schemaVersion: 1,
    operationId: init.operationId ?? "op-1",
    entityId: init.entityId,
    revision: init.revision ?? 0,
    sequence: init.sequence,
    attemptGeneration: init.attemptGeneration,
    kind: init.kind,
    payload: init.payload,
  };
}

/** Applies a snapshot to a fresh stream, the way `App` does on load. */
export function streamWith(tasks: readonly Task[]) {
  return reduceTaskStream(INITIAL_TASK_STREAM, { type: "snapshot", tasks });
}

/** Row component, exported only so the memo boundary can be asserted. */
export const taskRowComponent = TaskRow;
export const memoType = Symbol.for("react.memo");

const APP_INFO = {
  name: "SHTMCL",
  version: "0.1.0",
  contractVersion: 1,
  native: false,
  platform: "windows",
  arch: "x86_64",
};

export const TASKS: Record<string, Task> = {
  running: {
    id: "task-1",
    operationId: "op-1",
    instanceId: "instance-1",
    inputRevision: 7,
    attemptGeneration: 2,
    state: "running",
    phase: "download",
    progress: { current: 512000, total: 1024000, unit: "bytes" },
    cancelRequested: false,
  },
  queued: {
    id: "task-2",
    operationId: "op-2",
    attemptGeneration: 1,
    state: "queued",
    phase: "queue",
    cancelRequested: false,
  },
  succeeded: {
    id: "task-3",
    operationId: "op-3",
    instanceId: "instance-1",
    inputRevision: 9,
    attemptGeneration: 1,
    state: "succeeded",
    phase: "commit",
    progress: { current: 10, total: 10, unit: "items" },
    cancelRequested: false,
    evidence: ["binding_committed"],
  },
  failed: {
    id: "task-4",
    operationId: "op-4",
    attemptGeneration: 1,
    state: "failed",
    phase: "download",
    cancelRequested: false,
    error: {
      code: "download.source_unavailable",
      category: "network",
      retryable: true,
      userAction: "retry",
    },
  },
  unknownTotal: {
    id: "task-5",
    operationId: "op-5",
    attemptGeneration: 1,
    state: "running",
    phase: "verify",
    progress: { current: 3, total: null, unit: "steps" },
    cancelRequested: false,
  },
  cancelRequested: {
    id: "task-6",
    operationId: "op-6",
    attemptGeneration: 1,
    state: "running",
    phase: "verify",
    progress: { current: 4, total: 8, unit: "steps" },
    cancelRequested: true,
  },
};

interface ShellOptions {
  tasks?: readonly Task[];
  presentations?: readonly DownloadPresentation[];
  phase?: SnapshotPhase;
  feedback?: Readonly<Record<string, CancelFeedback>>;
  pendingCancel?: ReadonlySet<string>;
  preferences?: ShellPreferences;
  settings?: boolean;
}

const NO_FEEDBACK: Readonly<Record<string, CancelFeedback>> = {};
const NO_PENDING: ReadonlySet<string> = new Set<string>();
const noop = () => {};

function pageBody(page: PageId, options: ShellOptions) {
  const tasks = options.tasks ?? [];
  const phase = options.phase ?? "ready";

  switch (page) {
    case "start":
      return (
        <StartPage
          phase={phase}
          onRetry={noop}
          retrying={false}
          onNavigate={noop}
        />
      );
    case "instances":
      return (
        <InstancesPage phase={phase} onRetry={noop} retrying={false} />
      );
    case "multiplayer":
      return (
        <MultiplayerPage phase={phase} onRetry={noop} retrying={false} />
      );
    case "discover":
      return <DiscoverPage phase={phase} onRetry={noop} retrying={false} />;
    case "downloads":
      return <DownloadHubPage revision="" onAccepted={noop} onCreateEnvironment={noop} onViewTasks={noop} />;
    case "more":
      return <MorePage onNavigate={noop} />;
    case "tasks":
      return (
        <DownloadsPage
          tasks={tasks}
          presentations={options.presentations}
          hasSnapshot={phase !== "loading"}
          phase={phase}
          refreshing={false}
          feedback={options.feedback ?? NO_FEEDBACK}
          pendingCancel={options.pendingCancel ?? NO_PENDING}
          onCancel={noop}
          onRefresh={noop}
        />
      );
    case "settings":
      return (
        <SettingsPage
          preferences={options.preferences ?? DEFAULT_PREFERENCES}
          onChange={noop}
          appInfo={APP_INFO}
          environment="browser"
        />
      );
  }
}

/**
 * Shell markup identical in structure to `App.tsx`. The real `App` is also
 * rendered (see `renderApp`) for the first-paint measurement; this mirror lets
 * a test drive a page that would otherwise need a native snapshot.
 */
export function renderShell(page: PageId, options: ShellOptions = {}): string {
  const phase = options.phase ?? "ready";
  // Settings reads no snapshot data, so its failure is reported by the shell
  // banner — exactly like `App.tsx`.
  const showShellError = phase === "error" && page === "settings";

  return renderToStaticMarkup(
    <div className="app-shell">
      <a className="skip-link" href="#page-main">
        跳到主内容
      </a>
      <Sidebar page={page} onNavigate={noop} appInfo={APP_INFO} environment="browser" />
      <div className="app-shell__main">
        <TopBar
          title={pageCopy[page].title}
          environment="browser"
          loading={phase === "loading"}
          accountOpen={false}
          onOpenAccount={noop}
        />
        {showShellError ? (
          <div className="app-shell__banner">
            <Banner
              tone="error"
              title="本地状态读取失败"
              body="桌面核心没有返回快照。"
              actions={
                <Button variant="secondary" size="sm" icon="refresh" onClick={noop}>
                  刷新状态
                </Button>
              }
            />
          </div>
        ) : null}
        <main
          className="page-scroll"
          id="page-main"
          aria-labelledby="page-heading"
          tabIndex={-1}
        >
          {pageBody(page, options)}
        </main>
      </div>
      <AccountDialog open={false} onClose={noop} />
    </div>,
  );
}

/** The real shell component, in its pre-snapshot state (effects do not run). */
export function renderApp(): string {
  return renderToStaticMarkup(<App />);
}

export function renderTaskList(
  tasks: readonly Task[],
  feedback: Readonly<Record<string, CancelFeedback>> = NO_FEEDBACK,
  pendingCancel: ReadonlySet<string> = NO_PENDING,
): string {
  return renderToStaticMarkup(
    <TaskList
      tasks={tasks}
      feedback={feedback}
      pendingCancel={pendingCancel}
      onCancel={noop}
      onRefresh={noop}
    />,
  );
}

export function renderPageState(
  state:
    | "loading"
    | "empty"
    | "error"
    | "partial"
    | "busy"
    | "success"
    | "not_connected",
): string {
  return renderToStaticMarkup(
    <PageStateView state={state} loadingLabel="正在读取本地状态" onRetry={noop}>
      <p>content</p>
    </PageStateView>,
  );
}

export function renderDisabledButton(): string {
  return renderToStaticMarkup(
    <>
      <Button disabled reasonId="disabled-reason">
        添加账户
      </Button>
      <p id="disabled-reason">登录将在账户接入后可用。</p>
    </>,
  );
}

export { canRetryCachedDownload, canRetryInstanceInstall } from "../../../apps/desktop/src/ui/views/RetryCachedDownload";
export { retryInstallation } from "../../../apps/desktop/src/ui/views/install-recovery-api";

/** Opening tags = element count; used for the first-paint structure figure. */
export function countElements(markup: string): number {
  const matches = markup.match(/<[a-zA-Z][a-zA-Z0-9-]*/g);
  return matches ? matches.length : 0;
}

export function measure(): Record<string, { elements: number; bytes: number }> {
  const pages: PageId[] = ["start", "instances", "multiplayer", "discover", "downloads", "tasks", "more", "settings"];
  const result: Record<string, { elements: number; bytes: number }> = {};
  const push = (key: string, markup: string) => {
    result[key] = { elements: countElements(markup), bytes: Buffer.byteLength(markup, "utf8") };
  };

  push("app-first-paint", renderApp());
  for (const page of pages) {
    push(`page:${page}`, renderShell(page));
  }
  push("page:downloads-with-tasks", renderShell("downloads", { tasks: Object.values(TASKS) }));
  return result;
}

export * from "../../../apps/desktop/src/ui/views/game-selection";
export function renderSelectedStart(game: InstalledGame) { return renderToStaticMarkup(<StartPage phase="ready" libraryPhase="ready" game={game} connected onRetry={noop} retrying={false} onNavigate={noop} playerName="TestPlayer" />); }
export function renderSelectedGames(games: InstalledGame[], selectedGameId: string) { return renderToStaticMarkup(<InstancesPage phase="ready" libraryPhase="ready" games={games} selectedGameId={selectedGameId} connected onRetry={noop} retrying={false} onSelectGame={noop} onBack={noop} />); }
