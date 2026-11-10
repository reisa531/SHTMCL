import { useCallback, useEffect, useMemo, useReducer, useRef, useState } from "react";

import {
  isTauriRuntime,
  listenEvents,
  readAppInfo,
  readEvents,
  readShellSnapshot,
  requestCancel,
  dispatchCoreRead,
} from "./bridge";
import type { AppInfo, ShellSnapshot, Task } from "./contract";
import { navLabels, pageCopy, type PageId } from "./ux/copy";
import {
  applyPreferences,
  loadPreferences,
  savePreferences,
  type ShellPreferences,
} from "./ux/preferences";
import { Banner } from "./ui/components/Banner";
import { Button } from "./ui/components/Button";
import { downloadFlight, type MotionPoint } from "./ui/components/motion";
import { enterSurface, installSoftRipples } from "./ui/components/surface-motion";
import { CLOSED_OVERLAY, reduceOverlay } from "./ui/components/overlay";
import { uiLabels } from "./ui/labels";
import { DownloadHubPage } from "./ui/pages/DownloadHubPage";
import { MorePage } from "./ui/pages/MorePage";
import { DownloadsPage } from "./ui/pages/DownloadsPage";
import { InstancesPage } from "./ui/pages/InstancesPage";
import { CreateInstancePage } from "./ui/pages/CreateInstancePage";
import { parseDownloadPresentations, type DownloadPresentation } from "./ui/views/download-presentation";
import { downloadOperations } from "./ui/views/download-operations";
import type { CachedEnvironment } from "./ui/views/environment-api";
import { MultiplayerPage } from "./ui/pages/MultiplayerPage";
import { SettingsPage } from "./ui/pages/SettingsPage";
import { StartPage } from "./ui/pages/StartPage";
import { LaunchStatus } from "./ui/views/LaunchStatus";
import { useLaunchFlow } from "./ui/views/use-launch-flow";
import { useGameLibrary } from "./ui/views/use-game-library";
import { readSelectedGame, saveSelectedGame, resolveSelectedGame } from "./ui/views/game-selection";
import type { InstalledGame } from "./ui/views/game-install-api";
import { AccountDialog } from "./ui/views/AccountDialog";
import { gameLaunchApi, type LocalIdentity } from "./ui/views/game-launch-api";
import { Sidebar } from "./ui/views/Sidebar";
import { TopBar } from "./ui/views/TopBar";
import type { SnapshotPhase } from "./ui/views/page-state";
import { INITIAL_TASK_STREAM, reduceTaskStream } from "./ui/views/task-stream";
import {
  feedbackFromDecision,
  type CancelFeedback,
} from "./ui/views/task-format";

const PAGE_STORAGE_KEY = "shtmcl.shell.page.v1";
const EMPTY_PENDING: ReadonlySet<string> = new Set<string>();
const EMPTY_FEEDBACK: Readonly<Record<string, CancelFeedback>> = {};

function isPageId(value: unknown): value is PageId {
  return (
    typeof value === "string" &&
    Object.prototype.hasOwnProperty.call(navLabels, value)
  );
}

function loadPage(): PageId {
  try {
    const raw = window.localStorage.getItem(PAGE_STORAGE_KEY);
    return raw === "discover" ? "downloads" : isPageId(raw) ? raw : "start";
  } catch {
    return "start";
  }
}

export function App() {
  const shellRef = useRef<HTMLDivElement>(null);
  const contentRef = useRef<HTMLElement>(null);
  const [page, setPage] = useState<PageId>(loadPage);
  const [selectedGameId, setSelectedGameId] = useState<string | undefined>(readSelectedGame);
  const [libraryRevision, setLibraryRevision] = useState(0);
  const [downloadVisited, setDownloadVisited] = useState(false);
  const [gameRequest, setGameRequest] = useState(0);
  useEffect(() => { if (page === "downloads") setDownloadVisited(true); }, [page]);
  useEffect(() => { saveSelectedGame(selectedGameId); }, [selectedGameId]);
  const [creatingInstance, setCreatingInstance] = useState(false);
  const [targetInstanceId, setTargetInstanceId] = useState<string | undefined>();
  useEffect(() => shellRef.current ? installSoftRipples(shellRef.current) : undefined, []);
  useEffect(() => enterSurface(contentRef.current?.querySelector<HTMLElement>(":scope > div:not([hidden])") ?? null), [page, creatingInstance]);
  const [creationDraftOpen, setCreationDraftOpen] = useState(false);
  const [cachedEnvironment, setCachedEnvironment] = useState<CachedEnvironment | undefined>();
  const [presentations, setPresentations] = useState<DownloadPresentation[]>([]);
  const [downloadPhase, setDownloadPhase] = useState<SnapshotPhase>("loading");
  const [downloadFeedback, setDownloadFeedback] = useState<{ origin?: MotionPoint } | null>(null);
  const [downloadNotice, setDownloadNotice] = useState("");
  const playedDownloadFeedback = useRef<typeof downloadFeedback>(null);
  const [preferences, setPreferences] =
    useState<ShellPreferences>(loadPreferences);
  const [overlay, dispatchOverlay] = useReducer(reduceOverlay, CLOSED_OVERLAY);
  const [snapshot, setSnapshot] = useState<ShellSnapshot | null>(null);
  const [appInfo, setAppInfo] = useState<AppInfo | null>(null);
  const [identity, setIdentity] = useState<LocalIdentity | null>(null);
  const [phase, setPhase] = useState<SnapshotPhase>("loading");
  const [refreshing, setRefreshing] = useState(false);
  const [cancelFeedback, setCancelFeedback] =
    useState<Readonly<Record<string, CancelFeedback>>>(EMPTY_FEEDBACK);
  const [pendingCancel, setPendingCancel] =
    useState<ReadonlySet<string>>(EMPTY_PENDING);
  const [stream, dispatchStream] = useReducer(
    reduceTaskStream,
    INITIAL_TASK_STREAM,
  );

  const refreshIdentity = useCallback(async () => {
    if (!isTauriRuntime()) return;
    try {
      setIdentity(await gameLaunchApi().currentIdentity());
    } catch (error) {
      console.warn("SHTMCL player unavailable", error);
    }
  }, []);

  useEffect(() => { void refreshIdentity(); }, [refreshIdentity]);

  const loadSnapshot = useCallback(async () => {
    setRefreshing(true);
    try {
      const [info, nextSnapshot] = await Promise.all([
        readAppInfo(),
        readShellSnapshot(),
      ]);
      setAppInfo(info);
      setSnapshot(nextSnapshot);
      dispatchStream({ type: "snapshot", tasks: nextSnapshot.tasks });
      setPhase("ready");
    } catch (error) {
      // Technical detail stays in the console; the page shows the stable copy.
      console.warn("SHTMCL shell snapshot unavailable", error);
      setPhase("error");
    } finally {
      setRefreshing(false);
    }
  }, []);

  useEffect(() => {
    void loadSnapshot();
  }, [loadSnapshot]);

  // Live task events. The snapshot stays the baseline; each event moves one
  // row through the pure reducer. A browser preview has no core and therefore
  // no events, which must not be an error.
  useEffect(() => {
    let disposed = false;
    let stop: (() => void) | null = null;

    void listenEvents((event) => {
      if (!disposed) {
        dispatchStream({ type: "event", event });
      }
    })
      .then((unlisten) => {
        if (disposed) {
          unlisten();
        } else {
          stop = unlisten;
        }
      })
      .catch((error: unknown) => {
        console.warn("SHTMCL event subscription unavailable", error);
      });

    return () => {
      disposed = true;
      if (stop) {
        stop();
      }
    };
  }, []);

  // A sequence hole is repaired from the retained stream. A truncated replay
  // means history was clipped, so the snapshot is re-read instead of stitched.
  const gap = stream.gap;
  useEffect(() => {
    if (!gap) {
      return;
    }
    let cancelled = false;

    void (async () => {
      try {
        const replay = await readEvents(gap.operationId, gap.afterSequence);
        for (const event of replay.events) {
          dispatchStream({ type: "event", event });
        }
        if (replay.truncated) {
          await loadSnapshot();
        }
      } catch (error) {
        console.warn("SHTMCL event replay unavailable", error);
      } finally {
        if (!cancelled) {
          dispatchStream({ type: "gap-cleared", gap });
        }
      }
    })();

    return () => {
      cancelled = true;
    };
  }, [gap, loadSnapshot]);

  // An event naming a task the shell has never seen means the snapshot is stale.
  useEffect(() => {
    if (!stream.resyncRequested) {
      return;
    }
    dispatchStream({ type: "resync-handled" });
    void loadSnapshot();
  }, [stream.resyncRequested, loadSnapshot]);

  useEffect(() => {
    applyPreferences(preferences);
    savePreferences(preferences);
  }, [preferences]);

  useEffect(() => {
    try {
      window.localStorage.setItem(PAGE_STORAGE_KEY, page);
    } catch {
      // Persistence is optional; a locked profile must not break navigation.
    }
  }, [page]);

  const handleCancel = useCallback(async (task: Task) => {
    setPendingCancel((previous) => {
      const next = new Set(previous);
      next.add(task.id);
      return next;
    });

    let feedback: CancelFeedback;
    try {
      const outcome = await requestCancel(task.id, task.attemptGeneration);
      feedback = feedbackFromDecision(outcome.decision);
    } catch (error) {
      console.warn("SHTMCL cancel request unavailable", error);
      feedback = { status: "rejected", reason: uiLabels.taskCancelUnavailable };
    }

    setCancelFeedback((previous) => ({ ...previous, [task.id]: feedback }));
    setPendingCancel((previous) => {
      const next = new Set(previous);
      next.delete(task.id);
      return next;
    });
  }, []);

  const openAccount = useCallback(() => {
    dispatchOverlay({ type: "open", trigger: "shell-account-trigger" });
  }, []);

  const launchFlow = useLaunchFlow(stream.tasks, openAccount, loadSnapshot);
  const launchingId = launchFlow.busy ? launchFlow.intent?.game.id : undefined;
  const startLaunch = useCallback((game: InstalledGame) => {
    setSelectedGameId(game.id);
    launchFlow.start(game);
  }, [launchFlow.start]);
  const closeAccount = useCallback(() => {
    launchFlow.cancelIdentity();
    dispatchOverlay({ type: "dismiss" });
  }, [launchFlow.cancelIdentity]);
  const playerSelected = useCallback((selected: LocalIdentity) => {
    setIdentity(selected);
    launchFlow.resume();
  }, [launchFlow.resume]);

  /**
   * One stable refresh callback for the whole shell. Inline `() => …` handlers
   * here used to defeat every downstream memo boundary (evaluator defect D1).
   */
  const refresh = useCallback(() => {
    setLibraryRevision((value) => value + 1);
    void loadSnapshot();
  }, [loadSnapshot]);

  const installationAccepted = useCallback((origin?: MotionPoint) => {
    setCreatingInstance(false);
    setCreationDraftOpen(false);
    void loadSnapshot();
    setPage("tasks");
    setDownloadFeedback({ origin });
    setDownloadNotice("下载任务已加入");
  }, [loadSnapshot]);

  useEffect(() => {
    if (!downloadFeedback || playedDownloadFeedback.current === downloadFeedback) return;
    playedDownloadFeedback.current = downloadFeedback;
    return downloadFlight(downloadFeedback.origin);
  }, [downloadFeedback, page]);

  useEffect(() => {
    if (!downloadFeedback) return;
    const clearNotice = window.setTimeout(() => setDownloadNotice(""), 3000);
    return () => window.clearTimeout(clearNotice);
  }, [downloadFeedback]);

  const beginCreation = useCallback(() => {
    setTargetInstanceId(undefined);
    setCachedEnvironment(undefined);
    setPage("downloads");
    setGameRequest((value) => value + 1);
    setCreatingInstance(false);
  }, []);
  const createFromEnvironment = useCallback((environment: CachedEnvironment) => {
    setTargetInstanceId(undefined);
    setCachedEnvironment(environment); setCreationDraftOpen(true); setCreatingInstance(true); setPage("downloads");
  }, []);
  const leaveCreation = useCallback(() => setCreatingInstance(false), []);
  const viewInstance = useCallback((id: string) => {
    setSelectedGameId(id);
    setTargetInstanceId(id);
    setCreatingInstance(false);
    setPage("instances");
  }, []);
  const navigate = useCallback((next: PageId) => {
    setTargetInstanceId(undefined);
    setPage(next === "discover" ? "downloads" : next);
  }, []);
  const selectGame = useCallback((game: InstalledGame) => {
    setSelectedGameId(game.id);
    setPage("start");
  }, []);

  // Immutable operation metadata changes when a task is accepted, not on every
  // byte-progress event. Pair operation and task IDs across process restarts.
  const taskIdentity = stream.tasks.map((task) => `${task.operationId}:${task.id}`).join("|");
  useEffect(() => {
    if (!isTauriRuntime()) { setDownloadPhase("ready"); return; }
    let disposed = false;
    setDownloadPhase("loading");
    void dispatchCoreRead({
      requestId: crypto.randomUUID(),
      payload: { command: "launch.installations.list" },
    }).then((response) => {
      if (!disposed && response.type === "completed") {
        const result = response.result as { entries?: unknown } | null;
        if (!Array.isArray(result?.entries)) throw new Error("Invalid installation labels");
        setPresentations(parseDownloadPresentations(result?.entries));
        setDownloadPhase("ready");
      } else if (!disposed) {
        setDownloadPhase("error");
      }
    }).catch((error: unknown) => {
      console.warn("Installation labels unavailable", error);
      if (!disposed) setDownloadPhase("error");
    });
    return () => { disposed = true; };
  }, [taskIdentity, refreshing]);

  const environment: "native" | "browser" =
    snapshot?.mode ?? (isTauriRuntime() ? "native" : "browser");
  const tasks = stream.tasks;
  const gamesConnected = environment === "native" && !!snapshot?.modules.includes("instances") && !!snapshot?.modules.includes("launch");
  const completedTasks = tasks.filter((task) => ["succeeded", "failed", "cancelled"].includes(task.state)).map((task) => `${task.operationId}:${task.id}:${task.attemptGeneration}:${task.state}`).join("|");
  const library = useGameLibrary(gamesConnected, `${libraryRevision}:${completedTasks}`);
  const selectedGame = resolveSelectedGame(library.games, selectedGameId);
  useEffect(() => {
    if (gamesConnected && library.phase === "ready") setSelectedGameId(selectedGame?.id);
  }, [gamesConnected, library.phase, selectedGame?.id]);
  const activeDownloads = useMemo(() => downloadOperations(tasks, presentations).filter((operation) => operation.group === "active").length, [tasks, presentations]);

  const renderPage = () => {
    switch (page) {
      case "start":
        return (
          <StartPage
            phase={phase}
            onRetry={refresh}
            retrying={refreshing}
            onNavigate={navigate}
            connected={gamesConnected}
            game={selectedGame}
            libraryPhase={library.phase}
            playerName={identity?.username}
            onOpenAccount={() => dispatchOverlay({ type: "open", trigger: "start-account-trigger" })}
            onCreate={beginCreation}
            onLaunch={startLaunch}
            launchingId={launchingId}
          />
        );
      case "instances":
        return (
          <InstancesPage
            targetInstanceId={targetInstanceId}
            phase={phase}
            onRetry={refresh}
            retrying={refreshing}
            connected={gamesConnected}
            games={library.games}
            libraryPhase={library.phase}
            selectedGameId={selectedGame?.id}
            onSelectGame={selectGame}
            onBack={() => navigate("start")}
            onCreate={beginCreation}
            onLaunch={startLaunch}
            launchingId={launchingId}
          />
        );
      case "multiplayer":
        return (
          <MultiplayerPage
            phase={phase}
            onRetry={refresh}
            retrying={refreshing}
          />
        );
      case "discover":
      case "downloads":
        return null;
      case "more":
        return (
          <MorePage onNavigate={navigate} />
        );
      case "tasks":
        return (
          <DownloadsPage
            presentations={presentations}
            tasks={tasks}
            hasSnapshot={snapshot !== null}
            phase={phase === "error" ? phase : downloadPhase}
            refreshing={refreshing}
            feedback={cancelFeedback}
            pendingCancel={pendingCancel}
            onCancel={handleCancel}
            onRefresh={refresh}
            onAccepted={installationAccepted}
            onViewInstance={viewInstance}
          />
        );
      case "settings":
        return (
          <SettingsPage
            preferences={preferences}
            onChange={setPreferences}
            appInfo={appInfo}
            environment={environment}
          />
        );
    }
  };

  const pageBody = useMemo(renderPage, [
    page,
    phase,
    tasks,
    snapshot,
    refreshing,
    cancelFeedback,
    pendingCancel,
    handleCancel,
    refresh,
    preferences,
    appInfo,
    environment,
    installationAccepted,
    creatingInstance,
    beginCreation,
    leaveCreation,
    presentations,
    downloadPhase,
    createFromEnvironment,
    startLaunch,
    launchingId,
    targetInstanceId,
    viewInstance,
    navigate,
    gamesConnected,
    library.games,
    library.phase,
    selectedGame,
    identity,
    openAccount,
    selectGame,
  ]);

  // Data pages present their own error surface (retry lives next to the
  // message). Settings has no snapshot data, so it keeps the shell-level notice.
  const showShellError = phase === "error" && page === "settings";

  return (
    <div className="app-shell" ref={shellRef}>
      <a className="skip-link" href="#page-main">
        {uiLabels.skipToContent}
      </a>
      <p className="sr-only" role="status">{downloadNotice}</p>

      <Sidebar
        page={page}
        onNavigate={navigate}
        appInfo={appInfo}
        environment={environment}
        activeDownloads={activeDownloads}
      />

      <div className="app-shell__main">
        <TopBar
          title={pageCopy[page].title}
          environment={environment}
          loading={phase === "loading" || refreshing}
          accountOpen={overlay.open}
          onOpenAccount={openAccount}
          identityLabel={identity ? `本地玩家 · ${identity.username}` : undefined}
          accountSelected={identity !== null}
        />

        <LaunchStatus flow={launchFlow} onRefresh={refresh} refreshing={refreshing}
          onCancel={handleCancel} cancelling={!!launchFlow.task && pendingCancel.has(launchFlow.task.id)}
          feedback={launchFlow.task ? cancelFeedback[launchFlow.task.id] : undefined} />

        {showShellError ? (
          <div className="app-shell__banner">
            <Banner
              tone="error"
              title={uiLabels.errorTitle}
              body={uiLabels.errorBody}
              actions={
                <Button
                  variant="secondary"
                  size="sm"
                  icon="refresh"
                  pending={refreshing}
                  onClick={refresh}
                >
                  {uiLabels.taskRefresh}
                </Button>
              }
            />
          </div>
        ) : null}

        <main
          ref={contentRef}
          className="page-scroll"
          id="page-main"
          aria-labelledby={page === "downloads" && creatingInstance ? "create-instance-heading" : "page-heading"}
          tabIndex={-1}
        >
          <div hidden={page === "downloads"}>{pageBody}</div>
          {downloadVisited || page === "downloads" ? <div hidden={page !== "downloads" || creatingInstance}>
            <DownloadHubPage revision={completedTasks} games={library.games} gameRequest={gameRequest} onAccepted={installationAccepted} onCreateEnvironment={createFromEnvironment} onViewTasks={() => navigate("tasks")} />
          </div> : null}
          {creationDraftOpen ? (
            <div hidden={page !== "downloads" || !creatingInstance}>
              <CreateInstancePage onBack={leaveCreation} onAccepted={installationAccepted} environment={cachedEnvironment} />
            </div>
          ) : null}
        </main>
      </div>

      <AccountDialog open={overlay.open} onClose={closeAccount} onSelected={playerSelected} />
    </div>
  );
}
