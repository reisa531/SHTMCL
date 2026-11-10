import { useCallback, useEffect, useRef, useState } from "react";
import type { Task } from "../../contract";
import { gameInstallApi, type InstalledGame } from "./game-install-api";
import { gameLaunchApi, launchError, type AcceptedLaunch } from "./game-launch-api";
import { isTerminalState } from "./task-format";

interface LaunchIntent {
  game: InstalledGame;
  stage: "preparing" | "identity" | "accepted" | "error";
  accepted?: AcceptedLaunch;
  error?: string;
}

/** Keeps the user's target across navigation; task execution stays in the core. */
export function useLaunchFlow(tasks: readonly Task[], chooseIdentity: () => void, refresh: () => void) {
  const [intent, setIntent] = useState<LaunchIntent | null>(null);
  const current = useRef<LaunchIntent | null>(null);
  const taskList = useRef(tasks);
  taskList.current = tasks;
  const mounted = useRef(true);
  useEffect(() => {
    mounted.current = true;
    return () => { mounted.current = false; };
  }, []);

  const update = useCallback((next: LaunchIntent | null) => {
    current.current = next;
    if (mounted.current) setIntent(next);
  }, []);

  const submit = useCallback(async (game: InstalledGame) => {
    update({ game, stage: "preparing" });
    try {
      const api = gameLaunchApi();
      if (!await api.currentIdentity()) {
        if (!mounted.current) return;
        update({ game, stage: "identity" });
        chooseIdentity();
        return;
      }
      // The instance may have been removed or reconfigured while choosing a player.
      const latest = (await gameInstallApi().instances()).find((item) => item.id === game.id);
      if (!mounted.current) return;
      if (!latest || latest.version.gameVersion !== game.version.gameVersion
        || latest.version.loader !== game.version.loader
        || latest.version.loaderVersion !== game.version.loaderVersion) {
        update({ game, stage: "error", error: "实例已变更，请刷新实例后重新启动。" });
        return;
      }
      const accepted = await api.launch(game.id);
      if (!mounted.current) return;
      update({ game: latest, stage: "accepted", accepted });
      refresh();
    } catch (error) {
      if (mounted.current) update({ game, stage: "error", error: launchError(error) });
    }
  }, [chooseIdentity, refresh, update]);

  const start = useCallback((game: InstalledGame) => {
    const previous = current.current;
    if (previous && previous.stage !== "error") {
      const task = taskList.current.find((item) => item.id === previous.accepted?.taskId
        && item.operationId === previous.accepted?.operationId);
      if (!task || !isTerminalState(task.state)) return;
    }
    void submit(game);
  }, [submit]);

  const resume = useCallback(() => {
    const waiting = current.current;
    if (waiting?.stage === "identity") void submit(waiting.game);
  }, [submit]);
  const cancelIdentity = useCallback(() => {
    if (current.current?.stage === "identity") update(null);
  }, [update]);
  const task = tasks.find((item) => item.id === intent?.accepted?.taskId
    && item.operationId === intent?.accepted?.operationId);
  const busy = !!intent && intent.stage !== "error" && (!task || !isTerminalState(task.state));

  // One shell-level recovery read after a quiet period, not one poller per page.
  // Further recovery is explicit if the bridge is unavailable or the task disappeared.
  useEffect(() => {
    if (intent?.stage !== "accepted" || (task && isTerminalState(task.state))) return;
    const timer = window.setTimeout(refresh, 15000);
    return () => window.clearTimeout(timer);
  }, [intent, task, refresh]);

  const dismiss = useCallback(() => {
    const value = current.current;
    const observed = taskList.current.find((item) => item.id === value?.accepted?.taskId
      && item.operationId === value?.accepted?.operationId);
    if (value?.stage === "error" || (observed && isTerminalState(observed.state))) update(null);
  }, [update]);

  return { intent, task, busy, start, resume, cancelIdentity, dismiss };
}
